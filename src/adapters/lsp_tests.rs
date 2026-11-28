use super::*;

pub(crate) struct Workspace(pub PathBuf);
impl Workspace {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "xarness-lsp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    pub fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn config() -> LspServerConfig {
    serde_json::from_value(json!({
        "command":"python3", "args":[concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/lsp_server.py")],
        "filetypes":{"mock":"mock", "rs":"rust"}, "settings":{"mock":{"answer":42}}
    })).unwrap()
}

pub(crate) fn manager() -> Arc<Manager> {
    Arc::new(Manager::new(BTreeMap::from([("mock".into(), config())])).unwrap())
}

pub(crate) async fn state(c: &Connection) -> Value {
    c.request("test/state", json!({})).await.unwrap()
}

async fn until<F: Fn(&Value) -> bool>(c: &Connection, predicate: F) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = state(c).await;
            if predicate(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("mock condition did not arrive")
}

#[tokio::test]
async fn framing_unicode_unknown_headers_and_invalid_frames() {
    let value = json!({"text":"a😀é", "id": 7});
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &value).await.unwrap();
    let mut reader = BufReader::new(bytes.as_slice());
    assert_eq!(read_frame(&mut reader).await.unwrap(), value);
    for invalid in [
        b"Content-Type: application/json\r\n\r\n{}".as_slice(),
        b"Content-Length: 999999999\r\n\r\n",
        b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
        b"Content-Length: 10\r\n\r\n{}",
    ] {
        assert!(read_frame(&mut BufReader::new(invalid)).await.is_err());
    }
    let bytes =
        b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: 2\r\n\r\n{}";
    assert_eq!(
        read_frame(&mut BufReader::new(bytes.as_slice()))
            .await
            .unwrap(),
        json!({})
    );
}

#[test]
fn position_conversions_reject_partial_codepoints_and_roundtrip() {
    let text = "first\r\na😀éz\n";
    for (encoding, units) in [(Encoding::Utf16, 3), (Encoding::Utf8, 5)] {
        assert_eq!(
            to_position(text, 2, 3, encoding).unwrap(),
            Position::new(1, units)
        );
        for col in 1..=5 {
            let p = to_position(text, 2, col, encoding).unwrap();
            assert_eq!(from_position(text, p, encoding).unwrap(), (2, col));
        }
        assert!(from_position(text, Position::new(1, 2), encoding).is_err());
        assert!(to_position(text, 0, 1, encoding).is_err());
        assert!(to_position(text, 2, 6, encoding).is_err());
        assert_eq!(
            to_position(text, 3, 1, encoding).unwrap(),
            Position::new(2, 0)
        );
    }
}

#[tokio::test]
async fn shared_lazy_start_workspace_isolation_and_server_requests_during_initialize() {
    let root = Workspace::new();
    let other = Workspace::new();
    let manager = manager();
    assert!(manager.status().contains("idle"));
    assert!(!root.0.join("launches").exists());
    let path = root.file("a.mock", "a😀");
    let (a, b) = tokio::join!(
        manager.connection(&root.0, &path),
        manager.connection(&root.0, &path)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(
        std::fs::read_to_string(root.0.join("launches")).unwrap(),
        "start\n"
    );
    let different = manager
        .connection(&other.0, &other.file("a.mock", "b"))
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&a, &different));
    let status = manager.status();
    assert!(
        status.contains("definition=true, references=true"),
        "{status}"
    );
    let snapshot = state(&a).await;
    assert_eq!(snapshot["responses"]["config"]["result"], json!([42]));
    assert_eq!(snapshot["responses"]["edit"]["result"]["applied"], false);
    let (x, y) = tokio::join!(
        a.request("test/reverse", json!(1)),
        a.request("test/reverse", json!(2))
    );
    assert_eq!(x.unwrap(), json!(1));
    assert_eq!(y.unwrap(), json!(2));
    manager.shutdown().await;
    assert_eq!(
        std::fs::read_to_string(root.0.join("exited")).unwrap(),
        "clean"
    );
    assert!(lock(&a.pending).is_empty());
}

#[tokio::test]
async fn cancellation_timeout_crash_and_retry_are_distinct_from_empty_success() {
    let root = Workspace::new();
    let manager = manager();
    let path = root.file("a.mock", "a");
    let c = manager.connection(&root.0, &path).await.unwrap();
    let task_c = c.clone();
    let task = tokio::spawn(async move { task_c.request("test/hold", Value::Null).await });
    until(&c, |s| {
        s["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["method"] == "test/hold")
    })
    .await;
    task.abort();
    let _ = task.await;
    until(&c, |s| {
        s["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["method"] == "$/cancelRequest")
    })
    .await;
    assert!(lock(&c.pending).is_empty());
    let error = c
        .request_with_timeout("test/hold", Value::Null, Duration::from_millis(30))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timeout"));
    assert!(manager.status().contains("last error"));
    assert!(c.alive.load(Ordering::Acquire));
    assert!(
        c.query(
            &path,
            "a".into(),
            Position::new(0, 0),
            "textDocument/definition"
        )
        .await
        .unwrap()
        .is_null()
    );
    let error = c.request("test/crash", Value::Null).await.unwrap_err();
    assert!(error.to_string().contains("connection error"));
    let restarted = manager.connection(&root.0, &path).await.unwrap();
    assert!(!Arc::ptr_eq(&c, &restarted));
    assert_eq!(
        std::fs::read_to_string(root.0.join("launches")).unwrap(),
        "start\nstart\n"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn cancelled_waiter_does_not_cancel_shared_initialization() {
    let root = Workspace::new();
    let mut cfg = config();
    cfg.env.insert("MOCK_INIT_DELAY".into(), "0.15".into());
    let manager = Arc::new(Manager::new(BTreeMap::from([("mock".into(), cfg)])).unwrap());
    let path = root.file("a.mock", "a");
    let (m, r, p) = (manager.clone(), root.0.clone(), path.clone());
    let task = tokio::spawn(async move { m.connection(&r, &p).await });
    tokio::time::sleep(Duration::from_millis(40)).await;
    task.abort();
    let c = manager.connection(&root.0, &path).await.unwrap();
    assert!(c.supports("textDocument/definition"));
    assert_eq!(
        std::fs::read_to_string(root.0.join("launches")).unwrap(),
        "start\n"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn navigation_readiness_wait_is_cancellable_bounded_and_survives_disconnect() {
    let root = Workspace::new();
    let manager = manager();
    let path = root.file("a.mock", "a");
    let c = manager.connection(&root.0, &path).await.unwrap();
    c.request("test/progress", json!({"kind":"begin","title":"Indexing"}))
        .await
        .unwrap();
    let (connection, file) = (c.clone(), path.clone());
    let mut task = tokio::spawn(async move {
        connection
            .query(
                &file,
                "a".into(),
                Position::new(0, 0),
                "textDocument/references",
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut task)
            .await
            .is_err()
    );
    // Waiting for readiness must not send a semantic request or hold the document lock.
    assert!(lock(&c.pending).is_empty());
    assert!(c.documents.try_lock().unwrap().is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(c.alive.load(Ordering::Acquire));

    tokio::time::pause();
    let error = c
        .query(
            &path,
            "a".into(),
            Position::new(0, 0),
            "textDocument/references",
        )
        .await
        .unwrap_err();
    tokio::time::resume();
    assert!(
        error.to_string().contains("navigation timeout (30s)"),
        "{error}"
    );
    assert!(error.to_string().contains("Indexing"), "{error}");
    assert!(
        manager
            .status()
            .contains("last error: LSP navigation timeout")
    );
    c.request("test/progress", json!({"kind":"end"}))
        .await
        .unwrap();
    assert!(
        c.query(
            &path,
            "a".into(),
            Position::new(0, 0),
            "textDocument/references"
        )
        .await
        .unwrap()
        .is_null()
    );

    c.request("test/progress", json!({"kind":"begin","title":"Reloading"}))
        .await
        .unwrap();
    let (connection, file) = (c.clone(), path.clone());
    let mut task = tokio::spawn(async move {
        connection
            .query(
                &file,
                "a".into(),
                Position::new(0, 0),
                "textDocument/references",
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut task)
            .await
            .is_err()
    );
    c.request("test/crash", Value::Null).await.unwrap_err();
    let error = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("disconnected"), "{error}");
    let restarted = manager.connection(&root.0, &path).await.unwrap();
    assert!(!Arc::ptr_eq(&c, &restarted));
    manager.shutdown().await;
}

#[tokio::test]
async fn workspace_health_errors_are_explicit_and_warnings_remain_visible() {
    let root = Workspace::new();
    let manager = manager();
    let path = root.file("a.mock", "a");
    let c = manager.connection(&root.0, &path).await.unwrap();
    c.request(
        "test/status",
        json!({"health":"error", "quiescent":true, "message":"cargo metadata failed"}),
    )
    .await
    .unwrap();
    let error = c
        .query(
            &path,
            "a".into(),
            Position::new(0, 0),
            "textDocument/references",
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("LSP workspace error: cargo metadata failed"),
        "{error}"
    );
    assert!(manager.status().contains("health=error, quiescent=true"));
    c.request(
        "test/status",
        json!({"health":"warning", "quiescent":true, "message":"missing dependencies"}),
    )
    .await
    .unwrap();
    assert!(
        c.query(
            &path,
            "a".into(),
            Position::new(0, 0),
            "textDocument/references"
        )
        .await
        .unwrap()
        .is_null()
    );
    assert!(c.indexing().unwrap().contains("missing dependencies"));
    manager.shutdown().await;
}

#[tokio::test]
async fn full_and_incremental_sync_versions_save_and_deleted_open_documents() {
    for kind in [1, 2] {
        let root = Workspace::new();
        let mut cfg = config();
        cfg.env.insert("MOCK_SYNC".into(), kind.to_string());
        let manager = Manager::new(BTreeMap::from([("mock".into(), cfg)])).unwrap();
        let path = root.file("a.mock", "a😀\nlast");
        let second = root.file("b.mock", "before");
        let c = manager.connection(&root.0, &path).await.unwrap();
        for p in [&path, &second] {
            c.query(
                p,
                std::fs::read_to_string(p).unwrap(),
                Position::new(0, 0),
                "textDocument/definition",
            )
            .await
            .unwrap();
        }
        std::fs::write(&path, "updated").unwrap();
        std::fs::write(&second, "external edit").unwrap();
        c.query(
            &path,
            "updated".into(),
            Position::new(0, 0),
            "textDocument/references",
        )
        .await
        .unwrap();
        let s = state(&c).await;
        let uri = file_uri(&path).unwrap().to_string();
        let second_uri = file_uri(&second).unwrap().to_string();
        assert_eq!(s["documents"][&uri], json!({"text":"updated","version":2}));
        assert_eq!(s["documents"][&second_uri]["text"], "external edit");
        let changes: Vec<_> = s["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["method"] == "textDocument/didChange")
            .collect();
        assert_eq!(changes.len(), 2);
        let change = changes
            .iter()
            .find(|e| e["params"]["textDocument"]["uri"] == uri)
            .unwrap();
        assert_eq!(
            change["params"]["contentChanges"][0].get("range").is_some(),
            kind == 2
        );
        if kind == 2 {
            assert_eq!(
                change["params"]["contentChanges"][0]["range"]["end"],
                json!({"line":1,"character":4})
            );
        }
        assert!(
            s["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["method"] == "textDocument/didSave" && e["params"]["text"] == "updated")
        );
        assert_eq!(
            s["events"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|e| e["method"] == "textDocument/references")
                .unwrap()["params"]["context"]["includeDeclaration"],
            false
        );
        std::fs::remove_file(second).unwrap();
        c.query(
            &path,
            "updated".into(),
            Position::new(0, 0),
            "textDocument/definition",
        )
        .await
        .unwrap();
        assert!(state(&c).await["documents"].get(&second_uri).is_none());
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn native_watchers_deliver_create_change_rename_delete_and_unregister() {
    let root = Workspace::new();
    let manager = manager();
    let path = root.file("a.mock", "a");
    let c = manager.connection(&root.0, &path).await.unwrap();
    let has_event = |s: &Value, name: &str, kind: u64| {
        s["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["method"] == "workspace/didChangeWatchedFiles")
            .any(|e| {
                e["params"]["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["uri"].as_str().unwrap().ends_with(name) && v["type"] == kind)
            })
    };
    let created = root.file("created.mock", "one");
    until(&c, |s| has_event(s, "created.mock", 1)).await;
    std::fs::write(&created, "two").unwrap();
    until(&c, |s| has_event(s, "created.mock", 2)).await;
    let renamed = root.0.join("renamed.mock");
    std::fs::rename(&created, &renamed).unwrap();
    until(&c, |s| {
        has_event(s, "created.mock", 3) && has_event(s, "renamed.mock", 1)
    })
    .await;
    std::fs::remove_file(&renamed).unwrap();
    until(&c, |s| has_event(s, "renamed.mock", 3)).await;
    c.request("test/unregister", Value::Null).await.unwrap();
    assert!(lock(&c.watchers).is_empty());
    root.file("unwatched.mock", "x");
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!has_event(&state(&c).await, "unwatched.mock", 1));
    c.request("test/progress", json!({"kind":"begin","title":"Indexing"}))
        .await
        .unwrap();
    assert_eq!(c.indexing().as_deref(), Some("server progress: Indexing"));
    c.request("test/progress", json!({"kind":"end"}))
        .await
        .unwrap();
    assert!(c.indexing().is_none());
    manager.shutdown().await;
}

#[tokio::test]
async fn capabilities_utf8_and_spawn_failure() {
    let root = Workspace::new();
    let path = root.file("a.mock", "😀a");
    let mut cfg = config();
    cfg.env.insert("MOCK_UNSUPPORTED".into(), "1".into());
    cfg.env.insert("MOCK_ENCODING".into(), "utf-8".into());
    let manager = Manager::new(BTreeMap::from([("mock".into(), cfg)])).unwrap();
    let c = manager.connection(&root.0, &path).await.unwrap();
    assert_eq!(c.encoding(), Encoding::Utf8);
    assert!(
        c.query(
            &path,
            "😀a".into(),
            Position::new(0, 4),
            "textDocument/definition"
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("unsupported capability")
    );
    manager.shutdown().await;
    let mut cfg = config();
    cfg.command = "/nonexistent/xarness-test-lsp".into();
    let manager = Manager::new(BTreeMap::from([("mock".into(), cfg)])).unwrap();
    let error = manager.connection(&root.0, &path).await.err().unwrap();
    assert!(error.to_string().contains("connection error"));
    assert!(manager.status().contains("failed"));
    assert!(
        manager
            .connection(&root.0, &root.0.join("a.unknown"))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("unavailable")
    );
}

#[tokio::test]
async fn initialization_timeout_cleans_up_process_and_reports_its_phase() {
    let root = Workspace::new();
    let mut cfg = config();
    cfg.env.insert("MOCK_INIT_DELAY".into(), "60".into());
    let manager = Arc::new(Manager::new(BTreeMap::from([("mock".into(), cfg)])).unwrap());
    let path = root.file("a.mock", "a");
    let (m, r) = (manager.clone(), root.0.clone());
    let task = tokio::spawn(async move { m.connection(&r, &path).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.0.join("pid").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = task.await.unwrap().err().unwrap();
    tokio::time::resume();
    assert!(
        error.to_string().contains("initialization timeout"),
        "{error}"
    );
    assert!(
        manager
            .status()
            .contains("failed; last error: LSP initialization timeout")
    );
    #[cfg(target_os = "linux")]
    {
        let pid = std::fs::read_to_string(root.0.join("pid")).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed-out server process was not reaped");
    }
}

#[tokio::test]
async fn stderr_is_drained_and_bounded_without_polluting_stdout() {
    let root = Workspace::new();
    let mut cfg = config();
    cfg.env.insert("MOCK_STDERR".into(), "1".into());
    let manager = Manager::new(BTreeMap::from([("mock".into(), cfg)])).unwrap();
    let path = root.file("a.mock", "a");
    let c = manager.connection(&root.0, &path).await.unwrap();
    c.request("test/state", Value::Null).await.unwrap();
    assert!(lock(&c.info).stderr.len() <= LOG_BYTES);
    assert!(!lock(&c.info).stderr.is_empty());
    assert!(manager.status().contains("stderr (last 16384 bytes)"));
    manager.shutdown().await;
}
