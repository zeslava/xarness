"""Controlled stdio peer for the Rust LSP tests; no third party Python dependencies."""
import json
import os
import sys
import threading
import time

events = []
responses = {}
documents = {}
navigation = None
reverse = None
loading = False
load_on_query = os.environ.get("MOCK_LOAD_ON_QUERY") == "1"
content_modified = int(os.environ.get("MOCK_CONTENT_MODIFIED", "0"))
output_lock = threading.Lock()


def send(value):
    body = json.dumps(dict(jsonrpc="2.0", **value), ensure_ascii=False).encode()
    with output_lock:
        sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        sys.stdout.buffer.flush()


def reply(message, result):
    send(dict(id=message["id"], result=result))


def begin_loading():
    global loading
    loading = True
    send(dict(method="experimental/serverStatus", params=dict(health="ok", quiescent=False)))
    send(dict(method="$/progress", params=dict(token="index", value=dict(kind="begin", title="discovering sysroot"))))

    def finish():
        global loading
        delay = float(os.environ.get("MOCK_LOAD_DELAY", "0.12")) / 3
        time.sleep(delay)
        send(dict(method="$/progress", params=dict(token="index", value=dict(kind="end"))))
        # No progress token between phases does not mean the workspace is ready.
        time.sleep(delay)
        send(dict(method="$/progress", params=dict(token="index", value=dict(kind="begin", title="loading workspace"))))
        time.sleep(delay)
        loading = False
        send(dict(method="$/progress", params=dict(token="index", value=dict(kind="end"))))
        send(dict(method="experimental/serverStatus", params=dict(health="ok", quiescent=True)))

    threading.Thread(target=finish, daemon=True).start()


while True:
    length = None
    while True:
        header = sys.stdin.buffer.readline()
        if not header:
            sys.exit(0)
        if header == b"\r\n":
            break
        name, value = header.decode().split(":", 1)
        if name.lower() == "content-length":
            length = int(value)
    message = json.loads(sys.stdin.buffer.read(length))
    method = message.get("method")
    params = message.get("params", {})
    if method is None:
        responses[str(message["id"])] = message
        # Initialization requests are sent before the response and must be serviced independently.
        if "initialize" in globals() and "watch" in responses and "config" in responses and "edit" in responses:
            caps = dict(
                positionEncoding=os.environ.get("MOCK_ENCODING", "utf-16"),
                definitionProvider=os.environ.get("MOCK_UNSUPPORTED") != "1",
                referencesProvider=os.environ.get("MOCK_UNSUPPORTED") != "1",
                textDocumentSync=dict(openClose=True, change=int(os.environ.get("MOCK_SYNC", "2")), save=dict(includeText=True)),
            )
            result = dict(capabilities=caps)
            if os.environ.get("MOCK_SERVER_NAME"):
                result["serverInfo"] = dict(name=os.environ["MOCK_SERVER_NAME"])
            reply(initialize, result)
            del initialize
        continue
    events.append(message)
    if method == "initialize":
        with open("launches", "a") as log:
            log.write("start\n")
        with open("pid", "w") as log:
            log.write(str(os.getpid()))
        if os.environ.get("MOCK_STDERR"):
            sys.stderr.write("é" * 20000)
            sys.stderr.flush()
        time.sleep(float(os.environ.get("MOCK_INIT_DELAY", "0")))
        send(dict(id="config", method="workspace/configuration", params=dict(items=[dict(section="mock.answer")])))
        send(dict(id="edit", method="workspace/applyEdit", params=dict(edit={})))
        send(dict(id="watch", method="client/registerCapability", params=dict(registrations=[dict(id="files", method="workspace/didChangeWatchedFiles", registerOptions=dict(watchers=[dict(globPattern="**/*.mock")]))])))
        send(dict(id="progress", method="window/workDoneProgress/create", params=dict(token="index")))
        # Do not answer initialize until the client answers our requests.
        initialize = message
        continue
    elif method == "initialized":
        if os.environ.get("MOCK_LOAD_DELAY") and not load_on_query:
            begin_loading()
        elif os.environ.get("MOCK_SERVER_NAME") == "rust-analyzer":
            send(dict(method="experimental/serverStatus", params=dict(health="ok", quiescent=True)))
    elif method == "test/state":
        reply(message, dict(events=events, responses=responses, documents=documents))
    elif method == "test/setNavigation":
        navigation = params
        reply(message, None)
    elif method == "test/reverse":
        if reverse is None:
            reverse = message
        else:
            reply(message, params)
            reply(reverse, reverse["params"])
            reverse = None
    elif method == "test/hold":
        pass
    elif method == "test/crash":
        sys.exit(7)
    elif method == "test/progress":
        send(dict(method="$/progress", params=dict(token="index", value=params)))
        reply(message, None)
    elif method == "test/status":
        loading = not params["quiescent"]
        send(dict(method="experimental/serverStatus", params=params))
        reply(message, None)
    elif method == "test/unregister":
        send(dict(id="unwatch", method="client/unregisterCapability", params=dict(unregisterations=[dict(id="files", method="workspace/didChangeWatchedFiles")])))
        reply(message, None)
    elif method in ("textDocument/definition", "textDocument/references"):
        if content_modified:
            content_modified -= 1
            send(dict(id=message["id"], error=dict(code=-32801, message="workspace snapshot changed")))
        else:
            if load_on_query:
                load_on_query = False
                begin_loading()
            reply(message, None if loading else navigation)
    elif method == "textDocument/didOpen":
        doc = params["textDocument"]
        documents[doc["uri"]] = dict(text=doc["text"], version=doc["version"])
    elif method == "textDocument/didChange":
        doc = params["textDocument"]
        documents[doc["uri"]] = dict(text=params["contentChanges"][0]["text"], version=doc["version"])
    elif method == "textDocument/didClose":
        documents.pop(params["textDocument"]["uri"], None)
    elif method == "shutdown":
        reply(message, None)
    elif method == "exit":
        with open("exited", "w") as log:
            log.write("clean")
        sys.exit(0)
    elif "id" in message:
        reply(message, params)
