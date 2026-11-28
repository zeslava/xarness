use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::{FutureExt, future::BoxFuture};
use lsp_types::{GotoDefinitionResponse, Location};
use serde::Deserialize;
use serde_json::{Value, json};
use tree_sitter_tags::TagsContext;

use crate::adapters::lsp::{Encoding, Manager, from_position, to_position};
use crate::domain::ports::{Tool, ToolSpec};
use crate::repomap;

pub struct Navigation {
    pub manager: Arc<Manager>,
    pub references: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    name: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
}

impl Tool for Navigation {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.references {
                "find_usages"
            } else {
                "goto_definition"
            }
            .into(),
            description: format!(
                "{} using the configured language server. Pass a file path and either one exact definition name (scope-qualified when ambiguous), or line and column (one-based Unicode characters). Returns at most 50 sorted, deduplicated locations, with enclosing definitions and source lines when available. Unavailable LSP, unsupported capabilities and timeouts are explicit errors; there is no text-search fallback. External results include locations only.{}",
                if self.references {
                    "Find semantic references excluding the declaration"
                } else {
                    "Find the semantic definition of a symbol"
                },
                if self.references {
                    " Prefer this tool for symbol-usage questions. Without truncation or an incompleteness warning, report the result directly; do not cross-check it with grep or source reads unless requested details are missing."
                } else {
                    ""
                },
            ),
            parameters: json!({
                "type":"object", "properties": {
                    "path":{"type":"string","description":"File path relative to the working directory"},
                    "name":{"type":"string","description":"One exact definition name; scope separators may be . or ::"},
                    "line":{"type":"integer","minimum":1},
                    "column":{"type":"integer","minimum":1,"description":"One-based Unicode character column"}
                }, "required":["path"],
                "oneOf":[{"required":["name"],"not":{"anyOf":[{"required":["line"]},{"required":["column"]}]}},{"required":["line","column"],"not":{"required":["name"]}}],
                "additionalProperties":false
            }),
        }
    }

    fn mutating(&self) -> bool {
        false
    }

    fn run(&self, args: Value, root: &Path) -> BoxFuture<'static, Result<String>> {
        let (manager, root, references) =
            (self.manager.clone(), root.to_path_buf(), self.references);
        async move {
            let args: Args = super::parse(args)?;
            match (&args.name, args.line, args.column) {
                (Some(name), None, None) if !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || ",[]".contains(c)) => {},
                (None, Some(line), Some(column)) if line > 0 && column > 0 => {},
                _ => bail!("pass exactly one definition `name`, or both `line` and `column` (at least 1)"),
            }
            let path = super::resolve_path(&root, &args.path)?;
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let (mut line, mut column) = match address(&args, &text)? {
                Address::Position(line, column) => (line, column),
                Address::Ambiguous(candidates) => return Ok(format!("LSP symbol is ambiguous in {}; select a qualified name or position:\n{candidates}", args.path)),
            };
            let connection = manager.connection(&root, &path).await?;
            // Startup can take seconds; use the current disk snapshot for both addressing and sync.
            let current = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            if current != text {
                match address(&args, &current)? {
                    Address::Position(l,c) => { line = l; column = c; },
                    Address::Ambiguous(candidates) => return Ok(format!("LSP symbol is ambiguous in {}; select a qualified name or position:\n{candidates}", args.path)),
                }
            }
            let method = if references { "textDocument/references" } else { "textDocument/definition" };
            let position = to_position(&current, line, column, connection.encoding())?;
            let response = connection.query(&path, current, position, method).await?;
            let locations = locations(response, references)?;
            let mut out = render(&root, &connection.name, locations, connection.encoding())?;
            if let Some(progress) = connection.indexing() { out.push_str(&format!("\n[{progress}; results may be incomplete]")); }
            Ok(out)
        }.boxed()
    }
}

#[derive(Debug)]
enum Address {
    Position(usize, usize),
    Ambiguous(String),
}

fn address(args: &Args, text: &str) -> Result<Address> {
    if let Some(name) = &args.name {
        let config = repomap::languages()
            .for_path(&args.path)
            .context("name addressing is unavailable for this language; pass line and column")?;
        let defs = repomap::navigation_defs(&mut TagsContext::new(), config, text);
        let mut hits: Vec<_> = defs
            .iter()
            .filter(|d| super::symbol::matches(d, name))
            .collect();
        hits.sort_by_key(|d| d.name_byte);
        hits.dedup_by_key(|d| d.name_byte);
        if hits.is_empty() {
            bail!(
                "no definition `{name}` in {}; pass the position of a usage with line and column",
                args.path
            );
        }
        let position = |byte: usize| {
            let prefix = &text[..byte];
            (
                prefix.bytes().filter(|&b| b == b'\n').count() + 1,
                prefix
                    .rsplit('\n')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .count()
                    + 1,
            )
        };
        if hits.len() > 1 {
            let total = hits.len();
            let mut candidates = hits
                .into_iter()
                .take(50)
                .map(|d| {
                    let (line, col) = position(d.name_byte);
                    format!("{}:{line}:{col} {}", args.path, d.qualified_name)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if total > 50 {
                candidates.push_str(&format!("\n[truncated: showing 50 of {total} candidates]"));
            }
            return Ok(Address::Ambiguous(candidates));
        }
        let (line, column) = position(hits[0].name_byte);
        Ok(Address::Position(line, column))
    } else {
        let (line, column) = (
            args.line.context("missing line")?,
            args.column.context("missing column")?,
        );
        to_position(text, line, column, Encoding::Utf16)?;
        Ok(Address::Position(line, column))
    }
}

fn locations(value: Value, references: bool) -> Result<Vec<Location>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if references {
        return serde_json::from_value(value).context("invalid LSP references result");
    }
    Ok(
        match serde_json::from_value::<GotoDefinitionResponse>(value)
            .context("invalid LSP definition result")?
        {
            GotoDefinitionResponse::Scalar(l) => vec![l],
            GotoDefinitionResponse::Array(l) => l,
            GotoDefinitionResponse::Link(links) => links
                .into_iter()
                .map(|l| Location {
                    uri: l.target_uri,
                    range: l.target_selection_range,
                })
                .collect(),
        },
    )
}

fn render(
    root: &Path,
    server: &str,
    mut locations: Vec<Location>,
    encoding: Encoding,
) -> Result<String> {
    locations.sort_by(|a, b| {
        a.uri.as_str().cmp(b.uri.as_str()).then_with(|| {
            (
                a.range.start.line,
                a.range.start.character,
                a.range.end.line,
                a.range.end.character,
            )
                .cmp(&(
                    b.range.start.line,
                    b.range.start.character,
                    b.range.end.line,
                    b.range.end.character,
                ))
        })
    });
    locations.dedup();
    let total = locations.len();
    let mut out = format!("LSP {server}: {total} location(s)");
    // Read each local file once. Never open external result paths, including ../ in file URIs.
    let mut files = std::collections::HashMap::<PathBuf, Option<String>>::new();
    for location in locations.into_iter().take(50) {
        let position = location.range.start;
        let path = location
            .uri
            .to_file_path()
            .ok()
            .map(|p| super::lexical(root, &p.to_string_lossy()));
        let Some(path) = path else {
            out.push_str(&format!(
                "\n{}:{}:{} [non-file URI; LSP {:?} column]",
                location.uri,
                position.line + 1,
                position.character + 1,
                encoding
            ));
            continue;
        };
        let label = super::display(root, &path);
        if !path.starts_with(root) {
            out.push_str(&format!(
                "\n{label}:{}:{} [outside workspace; LSP {:?} column; content not read]",
                position.line + 1,
                position.character + 1,
                encoding
            ));
            continue;
        }
        let source = files
            .entry(path)
            .or_insert_with_key(|p| std::fs::read_to_string(p).ok());
        let Some(source) = source else {
            out.push_str(&format!(
                "\n{label}:{}:{} [content unavailable; LSP {:?} column]",
                position.line + 1,
                position.character + 1,
                encoding
            ));
            continue;
        };
        let (line, column) = from_position(source, position, encoding)?;
        let enclosing = repomap::languages().for_path(&label).and_then(|c| {
            repomap::navigation_defs(&mut TagsContext::new(), c, source)
                .into_iter()
                .filter(|d| d.start <= line && line <= d.end)
                .min_by_key(|d| d.end - d.start)
                .map(|d| d.qualified_name)
        });
        out.push_str(&format!("\n{label}:{line}:{column}"));
        if let Some(name) = enclosing {
            out.push_str(&format!(" [{name}]"));
        }
        out.push_str(&format!(
            " {}",
            super::clip(source.lines().nth(line - 1).unwrap_or_default().trim(), 300)
        ));
    }
    if total > 50 {
        out.push_str(&format!("\n[truncated: showing 50 of {total} locations]"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::lsp::{
        file_uri,
        tests::{Workspace, manager, state},
    };
    use crate::domain::message::Access;
    use crate::skills::Skills;
    use crate::tools::ToolRegistry;
    use lsp_types::{Position, Range};

    fn args(path: &str, name: &str) -> Args {
        Args {
            path: path.into(),
            name: Some(name.into()),
            line: None,
            column: None,
        }
    }

    #[test]
    fn names_reuse_symbol_scopes_and_exact_unicode_name_offsets() {
        let text = "struct A; struct B;\nimpl A { fn same() {} }\nimpl B { fn same() {} }\nfn café() {}\n#[cfg(test)] mod tests { fn test_name() {} }\n";
        let Address::Ambiguous(candidates) = address(&args("x.rs", "same"), text).unwrap() else {
            panic!("expected ambiguity");
        };
        assert!(candidates.contains("x.rs:2:13 A::same"), "{candidates}");
        assert!(candidates.contains("x.rs:3:13 B::same"));
        for (name, expected) in [
            ("A.same", (2, 13)),
            ("B::same", (3, 13)),
            ("café", (4, 4)),
            ("tests::test_name", (5, 29)),
        ] {
            let Address::Position(l, c) = address(&args("x.rs", name), text).unwrap() else {
                panic!("ambiguous {name}");
            };
            assert_eq!((l, c), expected, "{name}");
        }
        let Address::Ambiguous(candidates) = address(&args("x.rs", "B"), text).unwrap() else {
            panic!("struct and impl must be disambiguated");
        };
        assert!(candidates.contains("x.rs:1:18 B"), "{candidates}");
        assert!(
            address(&args("x.unknown", "same"), text)
                .unwrap_err()
                .to_string()
                .contains("pass line and column")
        );
    }

    #[test]
    fn definitions_accept_scalar_array_link_and_empty_but_reject_malformed() {
        let location = json!({"uri":"file:///x.rs","range":{"start":{"line":1,"character":2},"end":{"line":1,"character":5}}});
        assert_eq!(locations(location.clone(), false).unwrap().len(), 1);
        assert_eq!(locations(json!([location]), false).unwrap().len(), 1);
        let links = json!([{"targetUri":"file:///x.rs","targetRange":{"start":{"line":0,"character":0},"end":{"line":9,"character":0}},"targetSelectionRange":{"start":{"line":2,"character":3},"end":{"line":2,"character":7}}}]);
        assert_eq!(
            locations(links, false).unwrap()[0].range.start,
            Position::new(2, 3)
        );
        assert!(locations(Value::Null, false).unwrap().is_empty());
        assert!(locations(json!([]), true).unwrap().is_empty());
        assert!(locations(json!({"bad":true}), false).is_err());
    }

    #[test]
    fn output_is_sorted_deduplicated_bounded_and_never_reads_external_results() {
        let root = Workspace::new();
        let external = Workspace::new();
        let outside = external.file("secret.rs", "DO NOT INCLUDE THIS");
        let text = "fn first() {\n let s = \"😀é\";\n}\n";
        let file = root.file("a.rs", text);
        let local = Location::new(
            file_uri(&file).unwrap(),
            Range::new(Position::new(1, 14), Position::new(1, 15)),
        );
        let outside = Location::new(
            file_uri(&outside).unwrap(),
            Range::new(Position::new(0, 0), Position::new(0, 1)),
        );
        let out = render(
            &root.0,
            "mock",
            vec![outside, local.clone(), local],
            Encoding::Utf16,
        )
        .unwrap();
        assert!(out.starts_with("LSP mock: 2 location(s)"), "{out}");
        assert!(out.contains("a.rs:2:14 [first] let s = \"😀é\";"), "{out}");
        assert!(out.contains("content not read"));
        assert!(!out.contains("DO NOT INCLUDE"));
        let many: Vec<_> = (0..65)
            .rev()
            .map(|n| {
                Location::new(
                    file_uri(&root.0.join(format!("missing-{n:02}.rs"))).unwrap(),
                    Range::new(Position::new(0, 0), Position::new(0, 1)),
                )
            })
            .collect();
        let out = render(&root.0, "mock", many, Encoding::Utf16).unwrap();
        assert!(out.contains("truncated: showing 50 of 65"));
        assert!(out.find("missing-00").unwrap() < out.find("missing-49").unwrap());
        assert!(!out.contains("missing-50"));
    }

    #[tokio::test]
    async fn registry_keeps_schemas_access_allowlists_and_workspace_sharing() {
        let root = Workspace::new();
        let other = Workspace::new();
        root.file("x.rs", "fn target() {}\nfn caller() { target(); }\n");
        other.file("x.rs", "fn target() {}\n");
        let m = manager();
        let tools =
            ToolRegistry::new(root.0.clone(), Arc::new(Skills::default())).with_lsp(m.clone());
        let specs = || {
            tools
                .specs(Access::ReadOnly, None)
                .into_iter()
                .map(|s| (s.name, s.parameters))
                .collect::<Vec<_>>()
        };
        let before = specs();
        assert!(before.iter().any(|s| s.0 == "find_usages"));
        assert!(
            tools
                .specs(Access::Full, Some(&["read_file".into()]))
                .iter()
                .all(|t| t.name == "read_file")
        );
        assert!(
            tools
                .run(
                    Access::ReadOnly,
                    Some(&["read_file".into()]),
                    "find_usages",
                    r#"{"path":"x.rs","name":"target"}"#
                )
                .await
                .is_err()
        );
        let c = m.connection(&root.0, &root.0.join("x.rs")).await.unwrap();
        let location = json!({"uri":file_uri(&root.0.join("x.rs")).unwrap(),"range":{"start":{"line":1,"character":14},"end":{"line":1,"character":20}}});
        c.request("test/setNavigation", json!([location]))
            .await
            .unwrap();
        for access in [Access::Full, Access::Plan, Access::ReadOnly] {
            let output = tools
                .rooted(root.0.clone())
                .run(
                    access,
                    None,
                    "find_usages",
                    r#"{"path":"x.rs","name":"target"}"#,
                )
                .await
                .unwrap();
            assert!(output.contains("x.rs:2:15 [caller]"), "{output}");
        }
        let snapshot = state(&c).await;
        assert_eq!(
            snapshot["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["method"] == "textDocument/didOpen")
                .count(),
            1
        );
        tools
            .rooted(other.0.clone())
            .run(
                Access::ReadOnly,
                None,
                "goto_definition",
                r#"{"path":"x.rs","line":1,"column":4}"#,
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.0.join("launches")).unwrap(),
            "start\n"
        );
        assert_eq!(
            std::fs::read_to_string(other.0.join("launches")).unwrap(),
            "start\n"
        );
        assert_eq!(before, specs());
        assert!(tools.needs_approval(
            Access::Full,
            "goto_definition",
            r#"{"path":"../external.rs","line":1,"column":1}"#
        ));
        assert!(!tools.is_mutating("find_usages"));
        for invalid in [
            r#"{"path":"x.rs"}"#,
            r#"{"path":"x.rs","name":"target","line":1,"column":4}"#,
            r#"{"path":"x.rs","line":0,"column":1}"#,
            r#"{"path":"x.rs","line":1}"#,
            r#"{"path":"x.rs","name":"target,caller"}"#,
        ] {
            assert!(
                tools
                    .run(Access::Full, None, "goto_definition", invalid)
                    .await
                    .is_err(),
                "{invalid}"
            );
        }
        m.shutdown().await;
    }

    #[tokio::test]
    async fn missing_server_is_an_error_and_ambiguity_does_not_start_a_server() {
        let root = Workspace::new();
        root.file("x.rs", "impl A { fn same() {} }\nimpl B { fn same() {} }\n");
        let m = manager();
        let tool = Navigation {
            manager: m.clone(),
            references: false,
        };
        let out = tool
            .run(json!({"path":"x.rs","name":"same"}), &root.0)
            .await
            .unwrap();
        assert!(out.contains("ambiguous"));
        assert!(!root.0.join("launches").exists());
        root.file("x.txt", "same");
        assert!(
            tool.run(json!({"path":"x.txt","line":1,"column":1}), &root.0)
                .await
                .unwrap_err()
                .to_string()
                .contains("LSP unavailable")
        );
    }

    #[tokio::test]
    async fn first_navigation_waits_for_loading_and_retries_changed_snapshots() {
        for scenario in ["startup", "reload", "content_modified"] {
            let root = Workspace::new();
            root.file("x.rs", "fn target() {}\nfn caller() { target(); }\n");
            let mut cfg = crate::adapters::lsp::tests::config();
            cfg.env
                .insert("MOCK_SERVER_NAME".into(), "rust-analyzer".into());
            if scenario == "content_modified" {
                cfg.env.insert("MOCK_CONTENT_MODIFIED".into(), "1".into());
            } else {
                cfg.env.insert("MOCK_LOAD_DELAY".into(), "0.15".into());
                if scenario == "reload" {
                    cfg.env.insert("MOCK_LOAD_ON_QUERY".into(), "1".into());
                }
            }
            let manager = Arc::new(
                Manager::new(std::collections::BTreeMap::from([("mock".into(), cfg)])).unwrap(),
            );
            let c = manager
                .connection(&root.0, &root.0.join("x.rs"))
                .await
                .unwrap();
            let location = json!({"uri":file_uri(&root.0.join("x.rs")).unwrap(),"range":{"start":{"line":1,"character":14},"end":{"line":1,"character":20}}});
            c.request("test/setNavigation", json!([location]))
                .await
                .unwrap();
            let tools = ToolRegistry::new(root.0.clone(), Arc::new(Skills::default()))
                .with_lsp(manager.clone());
            // One tool call must produce the complete result; the model must not poll.
            let output = tools
                .run(
                    Access::ReadOnly,
                    None,
                    "find_usages",
                    r#"{"path":"x.rs","name":"target"}"#,
                )
                .await
                .unwrap();
            assert!(
                output.contains("x.rs:2:15 [caller]"),
                "{scenario}: {output}"
            );
            assert!(!output.contains("incomplete"), "{scenario}: {output}");
            let snapshot = state(&c).await;
            let requests = snapshot["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["method"] == "textDocument/references")
                .count();
            assert_eq!(
                requests,
                if scenario == "startup" { 1 } else { 2 },
                "{scenario}"
            );
            assert_eq!(
                snapshot["events"][0]["params"]["capabilities"]["experimental"]["serverStatusNotification"],
                true
            );
            manager.shutdown().await;
        }
    }

    #[tokio::test]
    #[ignore = "requires rust-analyzer and the project toolchain/dependencies"]
    async fn real_rust_analyzer_finds_config_load_on_first_call() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cfg = serde_json::from_value(json!({
            "command":"rust-analyzer", "filetypes":{"rs":"rust"},
            "env":{"RUSTC_WRAPPER":"", "CARGO_CACHE_RUSTC_INFO":"0", "CARGO_NET_OFFLINE":"true"}
        }))
        .unwrap();
        let manager = Arc::new(
            Manager::new(std::collections::BTreeMap::from([("rust".into(), cfg)])).unwrap(),
        );
        let tools = ToolRegistry::new(root, Arc::new(Skills::default())).with_lsp(manager.clone());
        let result = tools
            .run(
                Access::ReadOnly,
                None,
                "find_usages",
                r#"{"path":"src/config.rs","name":"Config::load"}"#,
            )
            .await;
        let status = manager.status();
        manager.shutdown().await;
        let output = result.unwrap_or_else(|e| panic!("{e:#}; {status}"));
        assert!(output.contains("src/main.rs:"), "{output}; {status}");
        assert!(output.contains("[main]"), "{output}");
        assert!(!output.contains("incomplete"), "{output}");
    }

    /// Requires installed rust-analyzer and gopls; uses the same registry calls and allowlist
    /// as a subagent, with no provider/API or model required.
    #[tokio::test]
    #[ignore = "requires rust-analyzer, gopls, Rust and Go toolchains"]
    async fn real_language_servers_use_the_same_navigation_interface_after_edits() {
        for (server, relative, source, definition, usage) in [
            (
                "rust-analyzer",
                "src/lib.rs",
                "pub struct A;\npub struct B;\nimpl A {\n    pub fn same() -> i32 { 1 }\n}\nimpl B {\n    pub fn same() -> i32 { 2 }\n}\npub fn call_a() -> i32 { A::same() }\npub fn call_b() -> i32 { B::same() }\n",
                "A::same",
                "same",
            ),
            (
                "gopls",
                "main.go",
                "package navigation\ntype A struct{}\ntype B struct{}\nfunc (a A) Same() int { return 1 }\nfunc (b B) Same() int { return 2 }\nfunc call_a() int { return A{}.Same() }\nfunc call_b() int { return B{}.Same() }\n",
                "A.Same",
                "Same",
            ),
        ] {
            let root = Workspace::new();
            std::fs::create_dir_all(root.0.join("src")).unwrap();
            root.file(
                "Cargo.toml",
                "[package]\nname = \"navigation\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            );
            root.file("go.mod", "module example.com/navigation\n\ngo 1.22\n");
            let path = root.file(relative, source);
            let cfg = serde_json::from_value(json!({
                "command":server, "filetypes": if server == "gopls" {json!({"go":"go"})} else {json!({"rs":"rust"})},
                "env":{"RUSTC_WRAPPER":"", "CARGO_NET_OFFLINE":"true", "GOTOOLCHAIN":"local", "GOPROXY":"off", "GOSUMDB":"off", "GOCACHE":root.0.join("go-cache"), "GOPATH":root.0.join("go")},
                "settings": if server == "gopls" {json!({"gopls":{}})} else {json!({"rust-analyzer":{"checkOnSave":false,"cargo":{"buildScripts":{"enable":false}},"procMacro":{"enable":false}}})}
            })).unwrap();
            let manager = Arc::new(
                Manager::new(std::collections::BTreeMap::from([(server.into(), cfg)])).unwrap(),
            );
            let tools = ToolRegistry::new(root.0.clone(), Arc::new(Skills::default()))
                .with_lsp(manager.clone());
            let allow = ["goto_definition".into(), "find_usages".into()];
            for edited in [false, true] {
                let text = if edited {
                    format!("\n\n{source}")
                } else {
                    source.to_string()
                };
                if edited {
                    tools
                        .run(
                            Access::Full,
                            None,
                            "write_file",
                            &json!({"path":relative,"content":text}).to_string(),
                        )
                        .await
                        .unwrap();
                }
                let (index, line) = text
                    .lines()
                    .enumerate()
                    .find(|(_, l)| l.contains("fn call_a") || l.contains("func call_a"))
                    .unwrap();
                let column = line[..line.find(usage).unwrap()].chars().count() + 1;
                let subagent = tools.rooted(root.0.clone());
                let output = subagent
                    .run(
                        Access::ReadOnly,
                        Some(&allow),
                        "find_usages",
                        &json!({"path":relative,"name":definition}).to_string(),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{server}: {e:#}; {}", manager.status()));
                assert!(
                    output.contains("[call_a]"),
                    "{server}: {output}; {}",
                    manager.status()
                );
                assert!(
                    output.contains(&format!("{relative}:{}:{column}", index + 1)),
                    "{server}: {output}"
                );
                assert!(!output.contains("[call_b]"), "{server}: {output}");
                let output = subagent
                    .run(
                        Access::ReadOnly,
                        Some(&allow),
                        "goto_definition",
                        &json!({"path":relative,"line":index+1,"column":column}).to_string(),
                    )
                    .await
                    .unwrap();
                assert!(
                    output.contains(&format!("[{definition}]")),
                    "{server}: {output}"
                );
                assert!(
                    !output.contains("B::same") && !output.contains("B.Same"),
                    "{output}"
                );
            }
            assert!(path.is_file());
            manager.shutdown().await;
        }
    }
}
