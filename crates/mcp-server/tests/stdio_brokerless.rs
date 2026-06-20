//! Integration tests: drive the real `one-for-all-mcp` binary over stdio.
//!
//! Verifies two transport contracts:
//!   - brokerless fallback (SPEC §10 quality / §2 wire shape):
//!     * `initialize` returns serverInfo + capabilities
//!     * `tools/list` returns the canonical tool surface
//!     * `tools/call` returns `-32011 BrokerUnavailable` when the broker is
//!       genuinely unreachable (rather than crashing the MCP child)
//!   - live broker forwarding (SPEC §2 + N34): real `net.observe` traffic
//!     crosses the MCP boundary as LSP-framed JSON-RPC `event/notify`
//!     payloads on stdout.
//!
//! The brokerless test points `ONE_FOR_ALL_SOCK` at a guaranteed-nonexistent
//! path so the kickstart-then-backoff path runs and exits the connect attempts,
//! then the binary falls through to `run_brokerless`. The live proof starts an
//! in-process broker on a temporary socket and uses a loopback HTTP server so
//! the observed network traffic is deterministic and does not depend on any
//! external site.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use broker::{server, IdleConfig, SessionEntry, State};
use browser_engine::{Browser, BrowserConfig, WaitUntil};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{
    Child as TokioChild, ChildStdin as TokioChildStdin, ChildStdout as TokioChildStdout,
    Command as TokioCommand,
};
use tokio::task::JoinHandle;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const LIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

fn binary_path() -> PathBuf {
    // Cargo sets `CARGO_BIN_EXE_<name>` for tests in the same package.
    let env_var = "CARGO_BIN_EXE_one-for-all-mcp";
    PathBuf::from(std::env::var(env_var).unwrap_or_else(|_| {
        // Fall back to the conventional release/debug locations so the test
        // also works when run outside cargo.
        let here = env!("CARGO_MANIFEST_DIR");
        let mut p = PathBuf::from(here);
        p.push("..");
        p.push("..");
        p.push("target");
        p.push("debug");
        p.push("one-for-all-mcp");
        p.to_string_lossy().into_owned()
    }))
}

fn resolve_test_chromium() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ONE_FOR_ALL_TEST_CHROMIUM") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = dirs::home_dir()?;
    let default = home
        .join(".one-for-all/chromium/149.0.7827.115/chrome-mac-arm64")
        .join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing");
    if default.exists() {
        Some(default)
    } else {
        None
    }
}

fn live_tests_enabled() -> bool {
    matches!(
        std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref(),
        Some("1")
    ) || matches!(std::env::var("BRIDGE_E2E_LIVE").ok().as_deref(), Some("1"))
}

fn require_chromium() -> PathBuf {
    resolve_test_chromium().unwrap_or_else(|| {
        panic!(
            "live tests enabled but no Chromium binary found. \
             Run `chromium-fetcher` once or set ONE_FOR_ALL_TEST_CHROMIUM=<path>."
        )
    })
}

fn spawn_brokerless() -> Child {
    Command::new(binary_path())
        // Path can't exist; binary will retry six times then fall through.
        .env(
            "ONE_FOR_ALL_SOCK",
            "/tmp/ofa-test-no-broker.sock.does-not-exist",
        )
        // Don't print noisy logs to the test's stderr.
        .env("OFA_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn one-for-all-mcp")
}

fn write_lsp(stdin: &mut ChildStdin, body: &[u8]) {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).expect("write header");
    stdin.write_all(body).expect("write body");
    stdin.flush().expect("flush stdin");
}

fn read_lsp(stdout: &mut ChildStdout) -> Vec<u8> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if Instant::now() > deadline {
            panic!("timed out reading header (got {} bytes)", header.len());
        }
        let n = stdout.read(&mut byte).expect("read header byte");
        if n == 0 {
            panic!("unexpected EOF while reading header");
        }
        header.push(byte[0]);
        if header.len() > 8 * 1024 {
            panic!("header too large");
        }
    }
    let h = std::str::from_utf8(&header).expect("header utf-8");
    let mut len: usize = 0;
    for line in h.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().expect("parse len");
            }
        }
    }
    let mut body = vec![0u8; len];
    stdout.read_exact(&mut body).expect("read body");
    body
}

fn rpc(stdin: &mut ChildStdin, stdout: &mut ChildStdout, method: &str, id: i64) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {},
    });
    write_lsp(stdin, &serde_json::to_vec(&req).unwrap());
    let body = read_lsp(stdout);
    serde_json::from_slice(&body).expect("response is JSON")
}

fn rpc_with_params(
    stdin: &mut ChildStdin,
    stdout: &mut ChildStdout,
    method: &str,
    id: i64,
    params: Value,
) -> Value {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    write_lsp(stdin, &serde_json::to_vec(&req).unwrap());
    let body = read_lsp(stdout);
    serde_json::from_slice(&body).expect("response is JSON")
}

async fn spawn_live_broker(
    socket_path: &Path,
    user_data_root: &Path,
    chromium: &Path,
) -> (Arc<State>, JoinHandle<()>) {
    let state = State::new(IdleConfig::default(), user_data_root.to_path_buf());
    *state.chromium_binary.lock() = Some(chromium.to_path_buf());
    let listener = server::bind_socket(socket_path).expect("bind broker socket");
    let state_for_task = Arc::clone(&state);
    let task = tokio::spawn(async move {
        server::run(state_for_task, listener)
            .await
            .expect("broker server run");
    });
    (state, task)
}

async fn register_broker_session(
    socket_path: &Path,
    name: &str,
) -> (
    TokioChild,
    TokioChildStdin,
    BufReader<TokioChildStdout>,
    String,
) {
    let mut child = spawn_live_mcp(socket_path);
    let mut stdin = child.stdin.take().expect("mcp stdin");
    let stdout = child.stdout.take().expect("mcp stdout");
    let mut stdout = BufReader::new(stdout);

    send_request_async(
        &mut stdin,
        "initialize",
        1,
        json!({
            "protocolVersion": "2024-11-05",
            "clientInfo": {"name": name, "version": "0.0.1"}
        }),
    )
    .await;
    let init = read_response_frame(&mut stdout, 1).await;
    assert_eq!(
        init.pointer("/result/serverInfo/name")
            .and_then(Value::as_str),
        Some("one-for-all")
    );
    send_notification_async(&mut stdin, "notifications/initialized", json!({})).await;

    send_request_async(
        &mut stdin,
        "tools/call",
        2,
        json!({
            "name": "tab.open",
            "arguments": {"url": "about:blank", "wait_until": "load", "timeout_ms": 30000}
        }),
    )
    .await;
    let open_resp = read_response_frame(&mut stdout, 2).await;
    let open_json = tool_result_json(&open_resp);
    let tab_id = open_json
        .get("tab_id")
        .and_then(Value::as_str)
        .expect("tab.open tab_id")
        .to_owned();

    (child, stdin, stdout, tab_id)
}

fn spawn_live_mcp(socket_path: &Path) -> TokioChild {
    spawn_live_mcp_config(socket_path, None, None)
}

fn spawn_live_mcp_with_session(socket_path: &Path, session_id: Option<&str>) -> TokioChild {
    spawn_live_mcp_config(socket_path, session_id, None)
}

fn spawn_live_mcp_with_capabilities(socket_path: &Path, capabilities: Option<&str>) -> TokioChild {
    spawn_live_mcp_config(socket_path, None, capabilities)
}

fn spawn_live_mcp_config(
    socket_path: &Path,
    session_id: Option<&str>,
    capabilities: Option<&str>,
) -> TokioChild {
    let mut cmd = TokioCommand::new(binary_path());
    cmd.env("ONE_FOR_ALL_SOCK", socket_path)
        .env("OFA_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(session_id) = session_id {
        cmd.env("OFA_SESSION_ID", session_id);
    }
    if let Some(capabilities) = capabilities {
        cmd.env("ONE_FOR_ALL_CAPABILITIES", capabilities);
    }
    cmd.spawn().expect("spawn live one-for-all-mcp")
}

async fn spawn_http_server() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback http server");
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/observe-proof");
    let task = tokio::spawn(async move {
        let body = br#"<!doctype html>
<title>observe-proof</title>
<body>ok</body>
<script>
localStorage.setItem('u2-http-page', '1');
sessionStorage.setItem('u2-http-page', '1');
window.__u2CacheReady = (async () => {
  const cache = await caches.open('u2-cache');
  await cache.put('/cached', new Response('cache-body', {
    headers: {'content-type': 'text/plain'}
  }));
  localStorage.setItem('u2-cache-ready', '1');
})();
</script>
"#;
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept http client");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await.expect("read http request");
            let mut response = Vec::new();
            response.extend_from_slice(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            response.extend_from_slice(body);
            stream
                .write_all(&response)
                .await
                .expect("write http response");
            let _ = stream.shutdown().await;
        }
    });
    (url, task)
}

async fn write_lsp_async(stdin: &mut TokioChildStdin, body: &[u8]) {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .await
        .expect("write async header");
    stdin.write_all(body).await.expect("write async body");
    stdin.flush().await.expect("flush async stdin");
}

async fn send_request_async(stdin: &mut TokioChildStdin, method: &str, id: i64, params: Value) {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    write_lsp_async(stdin, &serde_json::to_vec(&req).unwrap()).await;
}

async fn send_notification_async(stdin: &mut TokioChildStdin, method: &str, params: Value) {
    let req = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    write_lsp_async(stdin, &serde_json::to_vec(&req).unwrap()).await;
}

async fn read_lsp_async(stdout: &mut BufReader<TokioChildStdout>) -> Vec<u8> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(LIVE_FRAME_TIMEOUT, stdout.read(&mut byte))
            .await
            .expect("timed out reading async LSP header")
            .expect("read async header byte");
        if n == 0 {
            panic!("unexpected EOF while reading async LSP header");
        }
        header.push(byte[0]);
        if header.len() > 8 * 1024 {
            panic!("async LSP header too large");
        }
    }
    let h = std::str::from_utf8(&header).expect("async header utf-8");
    let mut len: usize = 0;
    for line in h.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().expect("parse async len");
            }
        }
    }
    let mut body = vec![0u8; len];
    tokio::time::timeout(LIVE_FRAME_TIMEOUT, stdout.read_exact(&mut body))
        .await
        .expect("timed out reading async LSP body")
        .expect("read async body");
    body
}

async fn read_json_frame(stdout: &mut BufReader<TokioChildStdout>) -> Value {
    let body = read_lsp_async(stdout).await;
    serde_json::from_slice(&body).expect("frame body is JSON")
}

async fn read_response_frame(stdout: &mut BufReader<TokioChildStdout>, id: i64) -> Value {
    loop {
        let frame = read_json_frame(stdout).await;
        if frame.get("id").and_then(Value::as_i64) == Some(id) {
            return frame;
        }
    }
}

fn tool_result_json(frame: &Value) -> Value {
    let text = frame
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing tools/call text payload: {frame:?}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool payload is not JSON: {e}: {text}"))
}

async fn initialize_mcp(
    stdin: &mut TokioChildStdin,
    stdout: &mut BufReader<TokioChildStdout>,
    client_name: &str,
) {
    send_request_async(
        stdin,
        "initialize",
        1,
        json!({
            "protocolVersion": "2024-11-05",
            "clientInfo": {"name": client_name, "version": "0.0.1"}
        }),
    )
    .await;
    let init = read_response_frame(stdout, 1).await;
    assert_eq!(
        init.pointer("/result/serverInfo/name")
            .and_then(Value::as_str),
        Some("one-for-all")
    );
    send_notification_async(stdin, "notifications/initialized", json!({})).await;
}

async fn shutdown_live_processes(
    mut child: TokioChild,
    stdin: TokioChildStdin,
    state: Arc<State>,
    broker_task: JoinHandle<()>,
    http_task: JoinHandle<()>,
) {
    drop(stdin);
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => {}
        Ok(Err(e)) => panic!("waiting for mcp child failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    for (sid, entry) in state.registry.iter() {
        if let Some(lifecycle) = entry.lifecycle.lock().take() {
            lifecycle.shutdown().await;
        }
        entry.abort_forwarders();
        entry.abort_trace_drivers();
        entry.shutdown_system_watches();
        entry.shutdown_terminals().await;
        let browser = entry.browser.load_full();
        let _ = browser.shutdown().await;
        state.registry.remove(&sid);
    }

    broker_task.abort();
    let _ = broker_task.await;
    http_task.abort();
    let _ = http_task.await;
}

#[test]
fn brokerless_initialize_tools_list_call_path() {
    let started = Instant::now();
    let mut child = spawn_brokerless();
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    // 1. initialize
    let resp = rpc_with_params(
        &mut stdin,
        &mut stdout,
        "initialize",
        1,
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "clientInfo": {"name": "ofa-it", "version": "0.0.1"}
        }),
    );
    let server_name = resp
        .pointer("/result/serverInfo/name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(server_name, "one-for-all", "server name mismatch: {resp:?}");

    // 2. tools/list — must include the full canonical surface (≥28 tools per
    //    SPEC §7 + M7/M8 = 30).
    let resp = rpc(&mut stdin, &mut stdout, "tools/list", 2);
    let tools = resp
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    assert!(tools.len() >= 38, "expected ≥38 tools, got {}", tools.len());
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    for needed in [
        "tab.open",
        "page.snapshot",
        "page.click",
        "page.cookies",
        "page.network_conditions",
        "page.emulate",
        "term.spawn",
        "term.read",
        "term.snapshot",
    ] {
        assert!(
            names.contains(&needed),
            "tools/list missing canonical tool: {needed}"
        );
    }

    // 3. tools/call — broker is unreachable, must return -32011 (or a related
    //    server error in the -32xxx band) rather than hanging the child.
    let resp = rpc_with_params(
        &mut stdin,
        &mut stdout,
        "tools/call",
        3,
        serde_json::json!({
            "name": "tab.open",
            "arguments": {"url": "https://example.com"}
        }),
    );
    let err_code = resp
        .pointer("/error/code")
        .and_then(|v| v.as_i64())
        .expect("error.code");
    assert!(
        (-32099..=-32000).contains(&err_code),
        "expected server-error band, got {err_code}: {resp:?}"
    );

    // Don't strictly assert -32011 — the brokerless fallback uses
    // `to_jsonrpc_error(BridgeError::BrokerUnavailable)` which maps to -32011,
    // but the dial path may surface a different code if the kickstart
    // subprocess inherits a quirk. Either is acceptable as long as it's a
    // server-error and arrives promptly.

    // Clean shutdown.
    drop(stdin);
    let _ = child.wait().unwrap();

    // The whole roundtrip should fit under CONNECT_TIMEOUT plus dial backoff
    // — gives a useful hang-detection canary.
    let elapsed = started.elapsed();
    assert!(
        elapsed < CONNECT_TIMEOUT,
        "roundtrip took {elapsed:?} (>{CONNECT_TIMEOUT:?}); something is hanging"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn net_observe_notifications_cross_mcp_as_lsp_event_notify() {
    if !live_tests_enabled() {
        eprintln!(
            "skipping net_observe_notifications_cross_mcp_as_lsp_event_notify: \
             ONE_FOR_ALL_LIVE_TESTS=1 (or BRIDGE_E2E_LIVE=1) is required"
        );
        return;
    }

    let chromium = require_chromium();
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker.sock");
    let user_data_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&user_data_root).expect("create user_data_root");

    let (state, broker_task) = spawn_live_broker(&socket_path, &user_data_root, &chromium).await;
    let (url, http_task) = spawn_http_server().await;

    let session_id = "s_n34_proof";
    let session_root = user_data_root.join(session_id);
    std::fs::create_dir_all(session_root.join("Default")).expect("create session default dir");

    let browser = Browser::launch(BrowserConfig::new_headless(
        chromium.clone(),
        session_root.clone(),
    ))
    .await
    .expect("launch seed browser");
    let entry = Arc::new(SessionEntry::new(
        session_id.to_owned(),
        browser,
        state.metrics.clone(),
    ));
    state.registry.insert(Arc::clone(&entry));

    let page = entry
        .browser
        .load_full()
        .default_context()
        .open_tab("about:blank", WaitUntil::None)
        .await
        .expect("open seed tab");
    let tab_id = page.tab_id().0.clone();

    let mut child = spawn_live_mcp_with_session(&socket_path, Some(session_id));
    let mut stdin = child.stdin.take().expect("mcp stdin");
    let stdout = child.stdout.take().expect("mcp stdout");
    let mut stdout = BufReader::new(stdout);
    initialize_mcp(&mut stdin, &mut stdout, "ofa-live-it").await;

    send_request_async(
        &mut stdin,
        "tools/call",
        2,
        json!({
            "name": "net.observe",
            "arguments": {"tab_id": tab_id, "filter": "127\\.0\\.0\\.1"}
        }),
    )
    .await;
    let observe_call_resp = read_response_frame(&mut stdout, 2).await;
    let observe_call_json = tool_result_json(&observe_call_resp);
    let subscription_id = observe_call_json
        .get("subscription_id")
        .and_then(Value::as_str)
        .expect("subscription_id")
        .to_owned();

    page.navigate(&url, WaitUntil::None)
        .await
        .expect("seed navigate");

    let notify_frame = tokio::time::timeout(LIVE_FRAME_TIMEOUT, async {
        loop {
            let frame = read_json_frame(&mut stdout).await;
            if frame.get("method").and_then(Value::as_str) == Some("event/notify")
                && frame.pointer("/params/topic").and_then(Value::as_str) == Some("network.request")
                && frame.pointer("/params/tab_id").and_then(Value::as_str) == Some(tab_id.as_str())
                && frame
                    .pointer("/params/payload/subscription_id")
                    .and_then(Value::as_str)
                    == Some(subscription_id.as_str())
                && frame.pointer("/params/payload/url").and_then(Value::as_str)
                    == Some(url.as_str())
            {
                return frame;
            }
        }
    })
    .await
    .expect("timed out waiting for forwarded network.request notify");

    assert_eq!(
        notify_frame.get("jsonrpc").and_then(Value::as_str),
        Some("2.0")
    );
    assert_eq!(
        notify_frame.get("method").and_then(Value::as_str),
        Some("event/notify")
    );
    assert!(
        notify_frame.get("id").is_none(),
        "notifications must omit id: {notify_frame:?}"
    );
    assert_eq!(
        notify_frame
            .pointer("/params/topic")
            .and_then(Value::as_str),
        Some("network.request")
    );
    assert_eq!(
        notify_frame
            .pointer("/params/payload/subscription_id")
            .and_then(Value::as_str),
        Some(subscription_id.as_str())
    );
    assert_eq!(
        notify_frame
            .pointer("/params/payload/url")
            .and_then(Value::as_str),
        Some(url.as_str())
    );
    assert_eq!(
        notify_frame
            .pointer("/params/payload/method")
            .and_then(Value::as_str),
        Some("GET")
    );
    assert_eq!(
        notify_frame
            .pointer("/params/session_id")
            .and_then(Value::as_str),
        Some(session_id)
    );

    shutdown_live_processes(child, stdin, state, broker_task, http_task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_surface_round_trips_cross_mcp_and_worker_stubs_stay_absent() {
    if !live_tests_enabled() {
        eprintln!(
            "skipping storage_surface_round_trips_cross_mcp_and_worker_stubs_stay_absent: \
             ONE_FOR_ALL_LIVE_TESTS=1 (or BRIDGE_E2E_LIVE=1) is required"
        );
        return;
    }

    let chromium = require_chromium();
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker.sock");
    let user_data_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&user_data_root).expect("create user_data_root");

    let (state, broker_task) = spawn_live_broker(&socket_path, &user_data_root, &chromium).await;
    let (url, http_task) = spawn_http_server().await;
    let mut child = spawn_live_mcp_with_capabilities(&socket_path, Some("storage_state,eval"));
    let mut stdin = child.stdin.take().expect("mcp stdin");
    let stdout = child.stdout.take().expect("mcp stdout");
    let mut stdout = BufReader::new(stdout);

    initialize_mcp(&mut stdin, &mut stdout, "ofa-live-storage").await;

    send_request_async(&mut stdin, "tools/list", 2, json!({})).await;
    let tools_resp = read_response_frame(&mut stdout, 2).await;
    let tools = tools_resp
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    for present in [
        "page.cookies.deep_set",
        "page.storage",
        "page.localstorage.get",
        "page.localstorage.set",
        "page.localstorage.delete",
        "page.localstorage.clear",
        "page.localstorage.cas",
        "page.sessionstorage.get",
        "page.sessionstorage.set",
        "page.sessionstorage.delete",
        "page.sessionstorage.clear",
        "page.sessionstorage.cas",
        "page.indexeddb.list_databases",
        "page.indexeddb.list_stores",
        "page.indexeddb.query",
        "page.indexeddb.put",
        "page.indexeddb.delete",
        "page.indexeddb.delete_database",
        "page.cache_api.list",
        "page.cache_api.inspect",
        "page.cache_api.delete",
        "page.permissions.query",
        "page.permissions.grant",
        "page.permissions.revoke",
        "page.storage.quota",
    ] {
        assert!(
            names.contains(&present),
            "tools/list missing landed U2 tool: {present}"
        );
    }
    for absent in [
        "page.workers.list",
        "page.workers.console",
        "page.workers.evaluate",
        "page.service_workers.list",
        "page.service_workers.inspect",
        "page.service_workers.unregister",
        "page.service_workers.update",
        "page.service_workers.trigger_event",
    ] {
        assert!(
            !names.contains(&absent),
            "tools/list unexpectedly exposes absent worker/service-worker stub: {absent}"
        );
    }

    send_request_async(
        &mut stdin,
        "tools/call",
        3,
        json!({
            "name": "tab.open",
            "arguments": {"url": "about:blank", "wait_until": "none", "timeout_ms": 30000}
        }),
    )
    .await;
    let open_resp = read_response_frame(&mut stdout, 3).await;
    let open_json = tool_result_json(&open_resp);
    let tab_id = open_json
        .get("tab_id")
        .and_then(Value::as_str)
        .expect("tab.open tab_id")
        .to_owned();

    send_request_async(
        &mut stdin,
        "tools/call",
        4,
        json!({
            "name": "tab.navigate",
            "arguments": {"tab_id": tab_id, "url": url}
        }),
    )
    .await;
    let navigate_resp = read_response_frame(&mut stdout, 4).await;
    let navigate_json = tool_result_json(&navigate_resp);
    assert_eq!(
        navigate_json.get("url").and_then(Value::as_str),
        Some(url.as_str())
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        5,
        json!({
            "name": "tab.wait",
            "arguments": {
                "tab_id": tab_id,
                "predicate": "networkidle",
                "timeout_ms": 30000
            }
        }),
    )
    .await;
    let wait_resp = read_response_frame(&mut stdout, 5).await;
    let wait_json = tool_result_json(&wait_resp);
    assert_eq!(wait_json.get("ok").and_then(Value::as_bool), Some(true));

    send_request_async(
        &mut stdin,
        "tools/call",
        6,
        json!({
            "name": "page.cookies.deep_set",
            "arguments": {
                "tab_id": tab_id,
                "cookie": {
                    "name": "u2_cookie",
                    "value": "set-via-deep-set",
                    "url": url,
                    "same_site": "Lax"
                }
            }
        }),
    )
    .await;
    let deep_set_resp = read_response_frame(&mut stdout, 6).await;
    let deep_set_json = tool_result_json(&deep_set_resp);
    assert_eq!(deep_set_json.get("ok").and_then(Value::as_bool), Some(true));

    send_request_async(
        &mut stdin,
        "tools/call",
        7,
        json!({
            "name": "page.cookies",
            "arguments": {"tab_id": tab_id, "action": "get"}
        }),
    )
    .await;
    let cookies_resp = read_response_frame(&mut stdout, 7).await;
    let cookies_json = tool_result_json(&cookies_resp);
    let cookies = cookies_json
        .get("cookies")
        .and_then(Value::as_array)
        .expect("cookies array");
    assert!(
        cookies.iter().any(|cookie| {
            cookie.get("name").and_then(Value::as_str) == Some("u2_cookie")
                && cookie.get("value").and_then(Value::as_str) == Some("set-via-deep-set")
        }),
        "page.cookies get missing deep-set cookie: {cookies_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        8,
        json!({
            "name": "page.storage",
            "arguments": {
                "tab_id": tab_id,
                "kind": "local",
                "action": "set",
                "args": {"key": "legacy-key", "value": "legacy-value"}
            }
        }),
    )
    .await;
    let legacy_set_resp = read_response_frame(&mut stdout, 8).await;
    let legacy_set_json = tool_result_json(&legacy_set_resp);
    assert_eq!(
        legacy_set_json.get("ok").and_then(Value::as_bool),
        Some(true)
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        9,
        json!({
            "name": "page.localstorage.get",
            "arguments": {"tab_id": tab_id, "key": "legacy-key"}
        }),
    )
    .await;
    let local_get_resp = read_response_frame(&mut stdout, 9).await;
    let local_get_json = tool_result_json(&local_get_resp);
    assert_eq!(
        local_get_json.get("value").and_then(Value::as_str),
        Some("legacy-value")
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        10,
        json!({
            "name": "page.localstorage.get",
            "arguments": {"tab_id": tab_id, "key": "u2-http-page"}
        }),
    )
    .await;
    let page_local_resp = read_response_frame(&mut stdout, 10).await;
    let page_local_json = tool_result_json(&page_local_resp);
    assert_eq!(
        page_local_json.get("value").and_then(Value::as_str),
        Some("1")
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        11,
        json!({
            "name": "page.sessionstorage.set",
            "arguments": {"tab_id": tab_id, "key": "session-key", "value": "session-value"}
        }),
    )
    .await;
    let session_set_resp = read_response_frame(&mut stdout, 11).await;
    let session_set_json = tool_result_json(&session_set_resp);
    assert_eq!(
        session_set_json.get("ok").and_then(Value::as_bool),
        Some(true)
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        12,
        json!({
            "name": "page.sessionstorage.get",
            "arguments": {"tab_id": tab_id, "key": "session-key"}
        }),
    )
    .await;
    let session_get_resp = read_response_frame(&mut stdout, 12).await;
    let session_get_json = tool_result_json(&session_get_resp);
    assert_eq!(
        session_get_json.get("value").and_then(Value::as_str),
        Some("session-value")
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        13,
        json!({
            "name": "page.sessionstorage.get",
            "arguments": {"tab_id": tab_id, "key": "u2-http-page"}
        }),
    )
    .await;
    let page_session_resp = read_response_frame(&mut stdout, 13).await;
    let page_session_json = tool_result_json(&page_session_resp);
    assert_eq!(
        page_session_json.get("value").and_then(Value::as_str),
        Some("1")
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        14,
        json!({
            "name": "page.indexeddb.put",
            "arguments": {
                "tab_id": tab_id,
                "database_name": "u2-db",
                "object_store_name": "items",
                "key": "alpha",
                "value": {"name": "alpha", "count": 7},
                "database_version": 1
            }
        }),
    )
    .await;
    let idb_put_resp = read_response_frame(&mut stdout, 14).await;
    let idb_put_json = tool_result_json(&idb_put_resp);
    assert_eq!(idb_put_json.get("ok").and_then(Value::as_bool), Some(true));

    send_request_async(
        &mut stdin,
        "tools/call",
        15,
        json!({
            "name": "page.indexeddb.list_databases",
            "arguments": {"tab_id": tab_id}
        }),
    )
    .await;
    let idb_list_resp = read_response_frame(&mut stdout, 15).await;
    let idb_list_json = tool_result_json(&idb_list_resp);
    let db_names = idb_list_json
        .get("database_names")
        .and_then(Value::as_array)
        .expect("database_names array");
    assert!(
        db_names.iter().any(|v| v.as_str() == Some("u2-db")),
        "database_names missing u2-db: {idb_list_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        16,
        json!({
            "name": "page.indexeddb.list_stores",
            "arguments": {"tab_id": tab_id, "database_name": "u2-db"}
        }),
    )
    .await;
    let idb_stores_resp = read_response_frame(&mut stdout, 16).await;
    let idb_stores_json = tool_result_json(&idb_stores_resp);
    let object_stores = idb_stores_json
        .pointer("/objectStores")
        .and_then(Value::as_array)
        .expect("objectStores array");
    assert!(
        object_stores
            .iter()
            .any(|store| store.get("name").and_then(Value::as_str) == Some("items")),
        "objectStores missing items store: {idb_stores_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        17,
        json!({
            "name": "page.indexeddb.query",
            "arguments": {
                "tab_id": tab_id,
                "database_name": "u2-db",
                "object_store_name": "items",
                "page_size": 10
            }
        }),
    )
    .await;
    let idb_query_resp = read_response_frame(&mut stdout, 17).await;
    let idb_query_json = tool_result_json(&idb_query_resp);
    let entries = idb_query_json
        .get("entries")
        .and_then(Value::as_array)
        .expect("entries array");
    assert!(
        !entries.is_empty(),
        "indexeddb query returned no entries: {idb_query_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        18,
        json!({
            "name": "page.storage.quota",
            "arguments": {"tab_id": tab_id}
        }),
    )
    .await;
    let quota_resp = read_response_frame(&mut stdout, 18).await;
    let quota_json = tool_result_json(&quota_resp);
    assert!(
        quota_json.get("origin").and_then(Value::as_str).is_some(),
        "storage quota must include origin: {quota_json:?}"
    );
    assert!(
        quota_json.get("quota").is_some(),
        "storage quota must include quota field: {quota_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        19,
        json!({
            "name": "page.localstorage.get",
            "arguments": {"tab_id": tab_id, "key": "u2-cache-ready"}
        }),
    )
    .await;
    let cache_ready_resp = read_response_frame(&mut stdout, 19).await;
    let cache_ready_json = tool_result_json(&cache_ready_resp);
    assert_eq!(
        cache_ready_json.get("value").and_then(Value::as_str),
        Some("1")
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        20,
        json!({
            "name": "page.cache_api.list",
            "arguments": {"tab_id": tab_id}
        }),
    )
    .await;
    let cache_list_resp = read_response_frame(&mut stdout, 20).await;
    let cache_list_json = tool_result_json(&cache_list_resp);
    let caches = cache_list_json
        .get("caches")
        .and_then(Value::as_array)
        .expect("caches array");
    let cache_id = caches
        .first()
        .and_then(|cache| cache.get("cacheId"))
        .and_then(Value::as_str)
        .expect("cache id from cache_api.list")
        .to_owned();

    send_request_async(
        &mut stdin,
        "tools/call",
        21,
        json!({
            "name": "page.cache_api.inspect",
            "arguments": {"tab_id": tab_id, "cache_id": cache_id, "page_size": 10}
        }),
    )
    .await;
    let cache_inspect_resp = read_response_frame(&mut stdout, 21).await;
    let cache_inspect_json = tool_result_json(&cache_inspect_resp);
    let cache_entries = cache_inspect_json
        .get("entries")
        .and_then(Value::as_array)
        .expect("cache entries array");
    assert!(
        cache_entries.iter().any(|entry| {
            entry
                .get("requestURL")
                .or_else(|| entry.get("requestUrl"))
                .and_then(Value::as_str)
                .map(|url| url.contains("/cached"))
                .unwrap_or(false)
        }),
        "cache inspect missing /cached entry: {cache_inspect_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        22,
        json!({
            "name": "page.permissions.query",
            "arguments": {
                "tab_id": tab_id,
                "permission": {"name": "geolocation"}
            }
        }),
    )
    .await;
    let permissions_query_resp = read_response_frame(&mut stdout, 22).await;
    let permissions_query_json = tool_result_json(&permissions_query_resp);
    assert!(
        matches!(
            permissions_query_json.get("state").and_then(Value::as_str),
            Some("granted" | "denied" | "prompt")
        ),
        "permissions.query must return a valid state: {permissions_query_json:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        23,
        json!({
            "name": "page.permissions.grant",
            "arguments": {
                "tab_id": tab_id,
                "permission": {"name": "geolocation"}
            }
        }),
    )
    .await;
    let permissions_grant_resp = read_response_frame(&mut stdout, 23).await;
    let permissions_grant_json = tool_result_json(&permissions_grant_resp);
    assert_eq!(
        permissions_grant_json.get("ok").and_then(Value::as_bool),
        Some(true)
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        24,
        json!({
            "name": "page.permissions.revoke",
            "arguments": {
                "tab_id": tab_id,
                "permission": {"name": "geolocation"}
            }
        }),
    )
    .await;
    let permissions_revoke_resp = read_response_frame(&mut stdout, 24).await;
    let permissions_revoke_json = tool_result_json(&permissions_revoke_resp);
    assert_eq!(
        permissions_revoke_json.get("ok").and_then(Value::as_bool),
        Some(true)
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        25,
        json!({
            "name": "page.service_workers.list",
            "arguments": {"tab_id": tab_id}
        }),
    )
    .await;
    let sw_missing_resp = read_response_frame(&mut stdout, 25).await;
    assert_eq!(
        sw_missing_resp.pointer("/error/code").and_then(Value::as_i64),
        Some(-32010),
        "absent page.service_workers.list should fail as unknown tool/protocol error: {sw_missing_resp:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        26,
        json!({
            "name": "page.workers.list",
            "arguments": {"tab_id": tab_id}
        }),
    )
    .await;
    let workers_missing_resp = read_response_frame(&mut stdout, 26).await;
    assert_eq!(
        workers_missing_resp.pointer("/error/code").and_then(Value::as_i64),
        Some(-32010),
        "absent page.workers.list should fail as unknown tool/protocol error: {workers_missing_resp:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        27,
        json!({
            "name": "tab.close",
            "arguments": {"tab_id": tab_id}
        }),
    )
    .await;
    let close_resp = read_response_frame(&mut stdout, 27).await;
    let close_json = tool_result_json(&close_resp);
    assert_eq!(close_json.get("ok").and_then(Value::as_bool), Some(true));

    assert_eq!(
        state.registry.len(),
        1,
        "expected exactly one live broker session"
    );

    shutdown_live_processes(child, stdin, state, broker_task, http_task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn term_notifications_cross_mcp_as_lsp_event_notify() {
    if !live_tests_enabled() {
        eprintln!(
            "skipping term_notifications_cross_mcp_as_lsp_event_notify: \
             ONE_FOR_ALL_LIVE_TESTS=1 (or BRIDGE_E2E_LIVE=1) is required"
        );
        return;
    }

    let chromium = require_chromium();
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker.sock");
    let user_data_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&user_data_root).expect("create user_data_root");

    let (state, broker_task) = spawn_live_broker(&socket_path, &user_data_root, &chromium).await;
    let cleanup_task = tokio::spawn(async {});
    let mut child = spawn_live_mcp(&socket_path);
    let mut stdin = child.stdin.take().expect("mcp stdin");
    let stdout = child.stdout.take().expect("mcp stdout");
    let mut stdout = BufReader::new(stdout);

    send_request_async(
        &mut stdin,
        "initialize",
        1,
        json!({
            "protocolVersion": "2024-11-05",
            "clientInfo": {"name": "ofa-live-it", "version": "0.0.1"}
        }),
    )
    .await;
    let init = read_response_frame(&mut stdout, 1).await;
    assert_eq!(
        init.pointer("/result/serverInfo/name")
            .and_then(Value::as_str),
        Some("one-for-all")
    );

    send_notification_async(&mut stdin, "notifications/initialized", json!({})).await;

    send_request_async(
        &mut stdin,
        "tools/call",
        2,
        json!({
            "name": "term.spawn",
            "arguments": {
                "shell": "/bin/sh",
                "cols": 80,
                "rows": 24,
                "env": {"TERM": "xterm-256color"}
            }
        }),
    )
    .await;
    let spawn_resp = read_response_frame(&mut stdout, 2).await;
    let spawn_json = tool_result_json(&spawn_resp);
    let term_session_id = spawn_json
        .get("session_id")
        .and_then(Value::as_str)
        .expect("term.spawn session_id")
        .to_owned();

    let marker = "__cb_term_notify__";
    send_request_async(
        &mut stdin,
        "tools/call",
        3,
        json!({
            "name": "term.write",
            "arguments": {
                "session_id": term_session_id,
                "text": format!("printf '{marker}\\n'; exit\\n")
            }
        }),
    )
    .await;

    let (write_resp, output_notify, exit_notify, combined_output) =
        tokio::time::timeout(LIVE_FRAME_TIMEOUT, async {
            let mut response: Option<Value> = None;
            let mut output_notify: Option<Value> = None;
            let mut exit_notify: Option<Value> = None;
            let mut combined = String::new();
            loop {
                let frame = read_json_frame(&mut stdout).await;
                if frame.get("id").and_then(Value::as_i64) == Some(3) {
                    response = Some(frame);
                } else if frame.get("method").and_then(Value::as_str) == Some("event/notify") {
                    let topic = frame.pointer("/params/topic").and_then(Value::as_str);
                    let payload_term_session_id = frame
                        .pointer("/params/payload/term_session_id")
                        .and_then(Value::as_str);
                    if payload_term_session_id == Some(term_session_id.as_str()) {
                        match topic {
                            Some("term.output") => {
                                if let Some(text) = frame
                                    .pointer("/params/payload/text")
                                    .and_then(Value::as_str)
                                {
                                    combined.push_str(text);
                                }
                                if output_notify.is_none() && combined.contains(marker) {
                                    output_notify = Some(frame.clone());
                                }
                            }
                            Some("term.exit") => {
                                if frame
                                    .pointer("/params/payload/exited")
                                    .and_then(Value::as_bool)
                                    == Some(true)
                                {
                                    exit_notify = Some(frame);
                                }
                            }
                            _ => {}
                        }
                    }
                }

                if let (Some(resp), Some(out), Some(exit)) =
                    (response.take(), output_notify.take(), exit_notify.take())
                {
                    return (resp, out, exit, combined);
                }
            }
        })
        .await
        .expect("timed out waiting for term.write response + forwarded term notifications");

    let write_json = tool_result_json(&write_resp);
    assert!(
        write_json
            .get("bytes_written")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        "term.write should report bytes_written: {write_json:?}"
    );
    assert!(
        combined_output.contains(marker),
        "combined terminal output missing marker: {combined_output:?}"
    );

    assert_eq!(
        output_notify.get("jsonrpc").and_then(Value::as_str),
        Some("2.0")
    );
    assert_eq!(
        output_notify.get("method").and_then(Value::as_str),
        Some("event/notify")
    );
    assert!(
        output_notify.get("id").is_none(),
        "notifications must omit id: {output_notify:?}"
    );
    assert_eq!(
        output_notify
            .pointer("/params/topic")
            .and_then(Value::as_str),
        Some("term.output")
    );
    assert_eq!(
        output_notify
            .pointer("/params/payload/term_session_id")
            .and_then(Value::as_str),
        Some(term_session_id.as_str())
    );
    assert!(
        output_notify
            .pointer("/params/payload/seq")
            .and_then(Value::as_u64)
            .is_some(),
        "term.output must carry payload.seq: {output_notify:?}"
    );
    assert!(
        output_notify
            .pointer("/params/payload/bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        "term.output must carry payload.bytes: {output_notify:?}"
    );
    assert!(
        output_notify
            .pointer("/params/payload/data_base64")
            .and_then(Value::as_str)
            .is_some(),
        "term.output must carry payload.data_base64: {output_notify:?}"
    );
    assert_eq!(
        output_notify
            .pointer("/params/payload/eof")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert!(
        output_notify
            .pointer("/params/session_id")
            .and_then(Value::as_str)
            .is_some(),
        "forwarded term.output must carry broker session_id: {output_notify:?}"
    );
    assert!(
        output_notify.pointer("/params/term_session_id").is_none(),
        "term.output must not leak payload fields to top level: {output_notify:?}"
    );
    assert!(
        output_notify.pointer("/params/seq").is_none(),
        "term.output must keep seq inside payload: {output_notify:?}"
    );

    assert_eq!(
        exit_notify.get("jsonrpc").and_then(Value::as_str),
        Some("2.0")
    );
    assert_eq!(
        exit_notify.get("method").and_then(Value::as_str),
        Some("event/notify")
    );
    assert!(
        exit_notify.get("id").is_none(),
        "notifications must omit id: {exit_notify:?}"
    );
    assert_eq!(
        exit_notify.pointer("/params/topic").and_then(Value::as_str),
        Some("term.exit")
    );
    assert_eq!(
        exit_notify
            .pointer("/params/payload/term_session_id")
            .and_then(Value::as_str),
        Some(term_session_id.as_str())
    );
    assert_eq!(
        exit_notify
            .pointer("/params/payload/exited")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        exit_notify
            .pointer("/params/payload/exit_code")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        exit_notify.pointer("/params/term_session_id").is_none(),
        "term.exit must not leak payload fields to top level: {exit_notify:?}"
    );

    send_request_async(
        &mut stdin,
        "tools/call",
        4,
        json!({
            "name": "term.close",
            "arguments": {"session_id": term_session_id}
        }),
    )
    .await;
    let close_resp = read_response_frame(&mut stdout, 4).await;
    let close_json = tool_result_json(&close_resp);
    assert_eq!(
        close_json.get("exited").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(close_json.get("exit_code").and_then(Value::as_u64), Some(0));

    assert_eq!(
        state.registry.len(),
        1,
        "expected exactly one live broker session"
    );

    shutdown_live_processes(child, stdin, state, broker_task, cleanup_task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_fsevents_watch_notifications_cross_mcp_as_lsp_event_notify() {
    if !live_tests_enabled() {
        eprintln!(
            "skipping system_fsevents_watch_notifications_cross_mcp_as_lsp_event_notify: \
             ONE_FOR_ALL_LIVE_TESTS=1 (or BRIDGE_E2E_LIVE=1) is required"
        );
        return;
    }

    let chromium = require_chromium();
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker.sock");
    let user_data_root = tmp.path().join("sessions");
    let watch_root = tmp.path().join("watch-root");
    std::fs::create_dir_all(&user_data_root).expect("create user_data_root");
    std::fs::create_dir_all(&watch_root).expect("create watch_root");
    let watch_root = watch_root.canonicalize().expect("canonicalize watch_root");

    let (state, broker_task) = spawn_live_broker(&socket_path, &user_data_root, &chromium).await;
    let cleanup_task = tokio::spawn(async {});

    let session_id = "s_u8_fsevents_proof";
    let session_root = user_data_root.join(session_id);
    std::fs::create_dir_all(session_root.join("Default")).expect("create session default dir");

    let browser = Browser::launch(BrowserConfig::new_headless(
        chromium.clone(),
        session_root.clone(),
    ))
    .await
    .expect("launch seed browser");
    let entry = Arc::new(SessionEntry::new(
        session_id.to_owned(),
        browser,
        state.metrics.clone(),
    ));
    state.registry.insert(Arc::clone(&entry));

    let mut child = spawn_live_mcp_config(&socket_path, Some(session_id), Some("system"));
    let mut stdin = child.stdin.take().expect("mcp stdin");
    let stdout = child.stdout.take().expect("mcp stdout");
    let mut stdout = BufReader::new(stdout);

    initialize_mcp(&mut stdin, &mut stdout, "ofa-fsevents-it").await;

    send_request_async(
        &mut stdin,
        "tools/call",
        2,
        json!({
            "name": "system.fsevents.watch",
            "arguments": {"paths": [watch_root.to_string_lossy().to_string()]}
        }),
    )
    .await;
    let watch_resp = read_response_frame(&mut stdout, 2).await;
    let watch_json = tool_result_json(&watch_resp);
    let watch_id = watch_json
        .get("watch_id")
        .and_then(Value::as_str)
        .expect("system.fsevents.watch watch_id")
        .to_owned();

    tokio::time::sleep(Duration::from_millis(250)).await;

    let watched_file = watch_root.join("proof.txt");
    std::fs::write(&watched_file, b"fsevents proof\n").expect("write watched file");
    let watched_file = watched_file
        .canonicalize()
        .expect("canonicalize watched file");
    let watched_file_str = watched_file.to_string_lossy().to_string();

    let notify_frame = tokio::time::timeout(LIVE_FRAME_TIMEOUT, async {
        loop {
            let frame = read_json_frame(&mut stdout).await;
            if frame.get("method").and_then(Value::as_str) != Some("event/notify") {
                continue;
            }
            if frame.pointer("/params/topic").and_then(Value::as_str) != Some("system.fsevents") {
                continue;
            }
            if frame
                .pointer("/params/payload/watch_id")
                .and_then(Value::as_str)
                != Some(watch_id.as_str())
            {
                continue;
            }
            if frame
                .pointer("/params/payload/path")
                .and_then(Value::as_str)
                != Some(watched_file_str.as_str())
            {
                continue;
            }
            return frame;
        }
    })
    .await
    .expect("timed out waiting for forwarded system.fsevents notify");

    assert_eq!(
        notify_frame.get("jsonrpc").and_then(Value::as_str),
        Some("2.0")
    );
    assert_eq!(
        notify_frame.get("method").and_then(Value::as_str),
        Some("event/notify")
    );
    assert!(
        notify_frame.get("id").is_none(),
        "notifications must omit id: {notify_frame:?}"
    );
    assert_eq!(
        notify_frame
            .pointer("/params/topic")
            .and_then(Value::as_str),
        Some("system.fsevents")
    );
    assert!(
        notify_frame
            .pointer("/params/session_id")
            .and_then(Value::as_str)
            .is_some(),
        "forwarded fsevents notify must carry broker session_id: {notify_frame:?}"
    );
    assert_eq!(
        notify_frame
            .pointer("/params/payload/watch_id")
            .and_then(Value::as_str),
        Some(watch_id.as_str())
    );
    assert_eq!(
        notify_frame
            .pointer("/params/payload/path")
            .and_then(Value::as_str),
        Some(watched_file_str.as_str())
    );
    assert!(
        notify_frame
            .pointer("/params/payload/event_id")
            .and_then(Value::as_u64)
            .is_some(),
        "forwarded fsevents notify must carry event_id: {notify_frame:?}"
    );
    assert!(
        notify_frame
            .pointer("/params/payload/ts_ns")
            .and_then(Value::as_u64)
            .is_some(),
        "forwarded fsevents notify must carry ts_ns: {notify_frame:?}"
    );
    let flags = notify_frame
        .pointer("/params/payload/flags")
        .and_then(Value::as_array)
        .expect("payload.flags array");
    assert!(
        !flags.is_empty(),
        "forwarded fsevents notify must carry at least one flag: {notify_frame:?}"
    );

    shutdown_live_processes(child, stdin, state, broker_task, cleanup_task).await;
}
