//! `net.*` handler. SPEC §7 (intercept/mock/observe) + SPEC §12 U3
//! deep-network surface (`net.intercept.fulfill_with_body`,
//! `net.intercept.modify_request`, `net.intercept.fail`, `net.replay`,
//! `net.websocket.observe`, `net.websocket.inject_frame`,
//! `net.eventsource.observe`, `net.har.export`, `net.proxy`,
//! `net.mitm_cert.install`).

use std::path::PathBuf;
use std::sync::Arc;

use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use browser_engine::network::{
    EsMessage, InterceptAction, MockResponse, NetworkRegistry, ProxyAuth, ProxyConfig,
    RequestOverrides, WsFrame,
};
use browser_engine::{Browser, Page};

use crate::protocol::ServerEvent;
use crate::recovery::RecoveredPageIdentity;
use crate::registry::{DurableNetworkObserve, SessionEntry};

use super::{current_session, current_state, locate_page, required_str, RouterError, ToolResult};

pub(super) async fn net_dispatch(browser: &Browser, method: &str, params: Value) -> ToolResult {
    use once_cell::sync::Lazy;
    use parking_lot::Mutex;
    static REGISTRY: Lazy<Mutex<Option<Arc<NetworkRegistry>>>> = Lazy::new(|| Mutex::new(None));

    // Lazy-init a per-process registry. (Spec allows shared.)
    let registry = {
        let mut g = REGISTRY.lock();
        match g.as_ref() {
            Some(r) => Arc::clone(r),
            None => {
                let r = NetworkRegistry::new();
                *g = Some(Arc::clone(&r));
                r
            }
        }
    };

    match method {
        // ----- SPEC §7 base surface -----
        "net.intercept" => {
            let page = locate_page(browser, &params)?;
            let pattern = required_str(&params, "pattern")?;
            // N32 — every unknown action variant becomes -32602 InvalidParams.
            // Default ("continue" when omitted) is still accepted; only an
            // explicit garbage value (e.g. "block", "Fulfill", a number)
            // errors out so callers learn about typos instead of silently
            // running with InterceptAction::Continue. Mirrors N20.
            let action = match params.get("action") {
                None | Some(Value::Null) => InterceptAction::Continue,
                Some(Value::String(s)) => match s.as_str() {
                    "continue" => InterceptAction::Continue,
                    "fulfill" => InterceptAction::Fulfill,
                    "fail" => InterceptAction::Fail,
                    other => {
                        return Err(RouterError::invalid_params(format!(
                            "unknown intercept action {other:?}; expected continue|fulfill|fail"
                        )));
                    }
                },
                Some(other) => {
                    return Err(RouterError::invalid_params(format!(
                        "unknown intercept action {other}; expected continue|fulfill|fail"
                    )));
                }
            };
            let id = page
                .net_intercept(&registry, pattern, action)
                .await
                .map_err(|e| RouterError::internal(format!("net.intercept: {e}")))?;
            Ok(json!({"handler_id": id}))
        }
        "net.mock" => {
            let page = locate_page(browser, &params)?;
            let pattern = required_str(&params, "url_pattern")?;
            let mock = parse_mock_response(params.get("response"));
            let id = page
                .net_mock(&registry, pattern, mock)
                .await
                .map_err(|e| RouterError::internal(format!("net.mock: {e}")))?;
            Ok(json!({"handler_id": id}))
        }
        "net.observe" => {
            let page = locate_page(browser, &params)?;
            let filter = params.get("filter").and_then(Value::as_str);
            let id = page.net_observe_subscription_id(&registry, filter);
            let regex = compile_filter(filter)?;
            let entry = current_session()
                .ok_or_else(|| RouterError::internal("missing current session".to_owned()))?;
            let session_id = entry.session_id.clone();
            let tab_id = page.tab_id().0.clone();
            entry.upsert_durable_network_observe(DurableNetworkObserve {
                subscription_id: id.clone(),
                tab_id: tab_id.clone(),
                filter: filter.map(str::to_owned),
            });
            let rx = page.net_observe_subscribe();
            spawn_network_observe_forwarder(&entry, rx, session_id, tab_id, id.clone(), regex);
            Ok(json!({"subscription_id": id}))
        }

        // ----- SPEC §12 U3 deep-network surface -----
        "net.intercept.fulfill_with_body" => {
            let page = locate_page(browser, &params)?;
            let pattern = required_str(&params, "pattern")?;
            let mock = parse_mock_response(params.get("response"));
            let id = page
                .net_intercept_fulfill_with_body(pattern, mock)
                .await
                .map_err(|e| {
                    RouterError::internal(format!("net.intercept.fulfill_with_body: {e}"))
                })?;
            Ok(json!({"handler_id": id}))
        }
        "net.intercept.modify_request" => {
            let page = locate_page(browser, &params)?;
            let pattern = required_str(&params, "pattern")?;
            let overrides = parse_request_overrides(params.get("overrides"));
            let id = page
                .net_intercept_modify_request(pattern, overrides)
                .await
                .map_err(|e| RouterError::internal(format!("net.intercept.modify_request: {e}")))?;
            Ok(json!({"handler_id": id}))
        }
        "net.intercept.fail" => {
            let page = locate_page(browser, &params)?;
            let pattern = required_str(&params, "pattern")?;
            let reason = params
                .get("error_reason")
                .and_then(Value::as_str)
                .unwrap_or("Failed")
                .to_owned();
            let id = page
                .net_intercept_fail(pattern, reason)
                .await
                .map_err(|e| RouterError::internal(format!("net.intercept.fail: {e}")))?;
            Ok(json!({"handler_id": id}))
        }
        "net.replay" => {
            let page = locate_page(browser, &params)?;
            let request_id = required_str(&params, "request_id")?;
            page.net_replay(request_id)
                .await
                .map_err(|e| RouterError::internal(format!("net.replay: {e}")))?;
            Ok(json!({"ok": true}))
        }
        "net.websocket.observe" => {
            let page = locate_page(browser, &params)?;
            let id = page.allocate_subscription_id("ws");
            let entry = current_session()
                .ok_or_else(|| RouterError::internal("missing current session".to_owned()))?;
            let rx = page.net_websocket_observe();
            let session_id = entry.session_id.clone();
            let tab_id = page.tab_id().0.clone();
            let handle = tokio::spawn(forward_websocket_observe(
                rx,
                entry.clone(),
                session_id,
                tab_id,
                id.clone(),
            ));
            entry.push_forwarder(handle);
            Ok(json!({"subscription_id": id, "channel": "ws"}))
        }
        "net.websocket.inject_frame" => {
            let page = locate_page(browser, &params)?;
            let url_substring = required_str(&params, "url_substring")?;
            let payload_b64 = required_str(&params, "payload_base64")?;
            page.net_websocket_inject_frame(url_substring, payload_b64)
                .await
                .map_err(|e| RouterError::internal(format!("net.websocket.inject_frame: {e}")))?;
            Ok(json!({"ok": true}))
        }
        "net.eventsource.observe" => {
            let page = locate_page(browser, &params)?;
            let id = page.allocate_subscription_id("es");
            let entry = current_session()
                .ok_or_else(|| RouterError::internal("missing current session".to_owned()))?;
            let rx = page.net_eventsource_observe();
            let session_id = entry.session_id.clone();
            let tab_id = page.tab_id().0.clone();
            let handle = tokio::spawn(forward_eventsource_observe(
                rx,
                entry.clone(),
                session_id,
                tab_id,
                id.clone(),
            ));
            entry.push_forwarder(handle);
            Ok(json!({"subscription_id": id, "channel": "eventsource"}))
        }
        "net.har.export" => {
            let page = locate_page(browser, &params)?;
            let since_ts = params
                .get("since_ts")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let har = page.net_har_export(since_ts);
            serde_json::to_value(&har)
                .map_err(|e| RouterError::internal(format!("net.har.export serialize: {e}")))
        }
        "net.proxy" => {
            // SPEC §12 U3 — proxy applies on **next** Browser::launch.
            // We can't hot-swap proxies on a running Chromium. Returning
            // accepted=true tells the caller the config is staged.
            let scheme = required_str(&params, "scheme")?.to_owned();
            let host = required_str(&params, "host")?.to_owned();
            let port = params
                .get("port")
                .and_then(Value::as_u64)
                .ok_or_else(|| RouterError::invalid_params("port required"))?
                as u16;
            let auth = params.get("auth").map(|a| ProxyAuth {
                user: a
                    .get("user")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                pass: a
                    .get("pass")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            });
            let bypass = params
                .get("bypass")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let cfg = ProxyConfig {
                scheme,
                host,
                port,
                auth,
                bypass,
            };
            let entry = current_session()
                .ok_or_else(|| RouterError::internal("missing current session".to_owned()))?;
            let session_root = session_root(&entry.session_id)?;
            entry
                .store_staged_proxy(&session_root, Some(cfg.clone()))
                .map_err(|e| RouterError::internal(format!("net.proxy persist: {e}")))?;
            Ok(json!({
                "accepted": true,
                "applies_at_next_launch": true,
                "argv": cfg.to_proxy_server_arg(),
            }))
        }
        "net.mitm_cert.install" => {
            // SPEC §12 U3 — explicit, documented refusal on macOS.
            // Chrome on macOS reads the native KeyChain, NOT NSS, so
            // silent install is impossible without user consent.
            let ca_pem = required_str(&params, "ca_pem")?;
            install_mitm_ca(ca_pem)
        }

        _ => Err(RouterError::method_not_found(method.to_owned())),
    }
}

fn compile_filter(filter: Option<&str>) -> Result<Option<Regex>, RouterError> {
    match filter {
        Some(expr) => Regex::new(expr).map(Some).map_err(|e| {
            RouterError::invalid_params(format!("invalid net.observe.filter regex: {e}"))
        }),
        None => Ok(None),
    }
}

fn session_root(session_id: &str) -> Result<PathBuf, RouterError> {
    let state =
        current_state().ok_or_else(|| RouterError::internal("missing current state".to_owned()))?;
    Ok(state.user_data_root.join(session_id))
}

fn observe_matches(regex: Option<&Regex>, url: &str) -> bool {
    regex.map(|r| r.is_match(url)).unwrap_or(true)
}

fn net_notify_event(topic: &str, session_id: &str, tab_id: &str, payload: Value) -> ServerEvent {
    ServerEvent {
        jsonrpc: "2.0".into(),
        method: "event/notify".into(),
        params: json!({
            "topic": topic,
            "session_id": session_id,
            "tab_id": tab_id,
            "payload": payload,
        }),
    }
}

pub(crate) fn replay_network_observe_subscriptions(
    entry: &Arc<SessionEntry>,
    restored_pages: &[Arc<Page>],
    previous_pages: &[RecoveredPageIdentity],
) {
    for observe in entry.durable_network_observe() {
        let regex = match compile_filter(observe.filter.as_deref()) {
            Ok(regex) => regex,
            Err(err) => {
                let _ = err;
                tracing::warn!(
                    session_id = %entry.session_id,
                    subscription_id = %observe.subscription_id,
                    "skipping restored net.observe subscription with invalid filter"
                );
                continue;
            }
        };

        let Some(restored) = match_observe_restored_page(&observe, restored_pages, previous_pages)
        else {
            tracing::warn!(
                session_id = %entry.session_id,
                subscription_id = %observe.subscription_id,
                original_tab_id = %observe.tab_id,
                "could not remap restored net.observe subscription"
            );
            continue;
        };

        let session_id = entry.session_id.clone();
        let tab_id = restored.tab_id().0.clone();
        entry.upsert_durable_network_observe(DurableNetworkObserve {
            subscription_id: observe.subscription_id.clone(),
            tab_id: tab_id.clone(),
            filter: observe.filter.clone(),
        });
        let rx = restored.net_observe_subscribe();
        spawn_network_observe_forwarder(
            entry,
            rx,
            session_id,
            tab_id,
            observe.subscription_id.clone(),
            regex,
        );
    }
}

fn spawn_network_observe_forwarder(
    entry: &Arc<SessionEntry>,
    rx: broadcast::Receiver<Value>,
    session_id: String,
    tab_id: String,
    subscription_id: String,
    regex: Option<Regex>,
) {
    let handle = tokio::spawn(forward_network_observe(
        rx,
        Arc::clone(entry),
        session_id,
        tab_id,
        subscription_id,
        regex,
    ));
    entry.push_forwarder(handle);
}

fn match_observe_restored_page<'a>(
    observe: &DurableNetworkObserve,
    restored_pages: &'a [Arc<Page>],
    previous_pages: &[RecoveredPageIdentity],
) -> Option<&'a Arc<Page>> {
    let previous = previous_pages
        .iter()
        .find(|page| page.tab_id == observe.tab_id)?;

    if let Some(page) = restored_pages
        .iter()
        .find(|page| page.target_id() == previous.target_id)
    {
        return Some(page);
    }

    let same_url: Vec<&Arc<Page>> = restored_pages
        .iter()
        .filter(|page| page.url() == previous.url)
        .collect();
    if same_url.len() == 1 {
        return same_url.into_iter().next();
    }

    let same_url_and_title: Vec<&Arc<Page>> = restored_pages
        .iter()
        .filter(|page| page.url() == previous.url && page.title() == previous.title)
        .collect();
    if same_url_and_title.len() == 1 {
        return same_url_and_title.into_iter().next();
    }

    None
}

fn net_observe_to_event(payload: &Value, subscription_id: &str) -> Option<(&'static str, Value)> {
    let kind = payload.get("kind")?.as_str()?;
    match kind {
        "request_will_be_sent" => Some((
            "network.request",
            json!({
                "subscription_id": subscription_id,
                "request_id": payload.get("request_id").cloned().unwrap_or(Value::Null),
                "url": payload.get("url").cloned().unwrap_or(Value::Null),
                "method": payload.get("method").cloned().unwrap_or(Value::Null),
                "timestamp": payload.get("timestamp").cloned().unwrap_or(Value::Null),
                "synthetic": payload.get("synthetic").cloned().unwrap_or(Value::Bool(false)),
            }),
        )),
        "response_received" => Some((
            "network.response",
            json!({
                "subscription_id": subscription_id,
                "request_id": payload.get("request_id").cloned().unwrap_or(Value::Null),
                "status": payload.get("status").cloned().unwrap_or(Value::Null),
                "headers": payload.get("headers").cloned().unwrap_or_else(|| json!({})),
                "mime_type": payload.get("mime_type").cloned().unwrap_or(Value::Null),
                "timestamp": payload.get("timestamp").cloned().unwrap_or(Value::Null),
            }),
        )),
        _ => None,
    }
}

async fn forward_network_observe(
    mut rx: broadcast::Receiver<Value>,
    entry: Arc<SessionEntry>,
    session_id: String,
    tab_id: String,
    subscription_id: String,
    regex: Option<Regex>,
) {
    loop {
        match rx.recv().await {
            Ok(payload) => {
                let url = payload.get("url").and_then(Value::as_str).unwrap_or("");
                if !observe_matches(regex.as_ref(), url) {
                    continue;
                }
                if let Some((topic, normalized)) = net_observe_to_event(&payload, &subscription_id)
                {
                    let _ =
                        entry.try_push(net_notify_event(topic, &session_id, &tab_id, normalized));
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

async fn forward_websocket_observe(
    mut rx: broadcast::Receiver<WsFrame>,
    entry: Arc<SessionEntry>,
    session_id: String,
    tab_id: String,
    subscription_id: String,
) {
    loop {
        match rx.recv().await {
            Ok(frame) => {
                let mut payload = serde_json::to_value(frame).unwrap_or_else(|_| json!({}));
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert(
                        "subscription_id".to_owned(),
                        Value::String(subscription_id.clone()),
                    );
                }
                let _ = entry.try_push(net_notify_event(
                    "network.websocket",
                    &session_id,
                    &tab_id,
                    payload,
                ));
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

async fn forward_eventsource_observe(
    mut rx: broadcast::Receiver<EsMessage>,
    entry: Arc<SessionEntry>,
    session_id: String,
    tab_id: String,
    subscription_id: String,
) {
    loop {
        match rx.recv().await {
            Ok(msg) => {
                let mut payload = serde_json::to_value(msg).unwrap_or_else(|_| json!({}));
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert(
                        "subscription_id".to_owned(),
                        Value::String(subscription_id.clone()),
                    );
                }
                let _ = entry.try_push(net_notify_event(
                    "network.eventsource",
                    &session_id,
                    &tab_id,
                    payload,
                ));
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

fn parse_mock_response(v: Option<&Value>) -> MockResponse {
    let resp = v.cloned().unwrap_or(json!({}));
    MockResponse {
        status: resp.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        headers: resp
            .get("headers")
            .and_then(Value::as_array)
            .map(|hs| {
                hs.iter()
                    .filter_map(|h| {
                        let arr = h.as_array()?;
                        Some((
                            arr.first()?.as_str()?.to_owned(),
                            arr.get(1)?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        body_base64: resp
            .get("body_base64")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    }
}

fn parse_request_overrides(v: Option<&Value>) -> RequestOverrides {
    let v = v.cloned().unwrap_or(json!({}));
    RequestOverrides {
        url: v.get("url").and_then(Value::as_str).map(str::to_owned),
        method: v.get("method").and_then(Value::as_str).map(str::to_owned),
        headers: v
            .get("headers")
            .and_then(Value::as_array)
            .map(|hs| {
                hs.iter()
                    .filter_map(|h| {
                        let arr = h.as_array()?;
                        Some((
                            arr.first()?.as_str()?.to_owned(),
                            arr.get(1)?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        post_data_base64: v
            .get("post_data_base64")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ErrorCode;

    #[test]
    fn compile_filter_rejects_invalid_regex() {
        let err = compile_filter(Some("(")).expect_err("invalid regex should error");
        assert!(matches!(err.code, ErrorCode::InvalidParams));
    }

    #[test]
    fn observe_mapping_adds_subscription_id() {
        let payload = json!({
            "kind": "request_will_be_sent",
            "request_id": "R1",
            "url": "https://example.com/a",
            "method": "GET",
            "timestamp": 123.4,
        });
        let (topic, mapped) = net_observe_to_event(&payload, "s_1").expect("mapped event");
        assert_eq!(topic, "network.request");
        assert_eq!(
            mapped.get("subscription_id").and_then(Value::as_str),
            Some("s_1")
        );
        assert_eq!(mapped.get("request_id").and_then(Value::as_str), Some("R1"));
    }
}

#[cfg(target_os = "linux")]
fn install_mitm_ca(ca_pem: &str) -> ToolResult {
    use std::io::Write as _;
    use std::process::Command;
    // Probe for `certutil` via PATH lookup. Production deployments need
    // libnss3-tools installed; we surface a clear error if it's missing
    // rather than silently failing.
    let probe = Command::new("certutil")
        .arg("--help")
        .output()
        .map_err(|e| {
            RouterError::internal(format!(
                "net.mitm_cert.install: certutil unavailable ({e}); install libnss3-tools"
            ))
        })?;
    if !probe.status.success() && probe.status.code() != Some(1) {
        // certutil prints help with exit 1; treat anything else as missing.
    }
    let mut tmp = tempfile::NamedTempFile::new()
        .map_err(|e| RouterError::internal(format!("temp ca: {e}")))?;
    tmp.write_all(ca_pem.as_bytes())
        .map_err(|e| RouterError::internal(format!("write ca: {e}")))?;
    let nss_dir = std::env::var("CHROME_USER_DATA_DIR")
        .map(|d| format!("sql:{d}/Default/.pki/nssdb"))
        .unwrap_or_else(|_| "sql:.pki/nssdb".to_owned());
    let status = Command::new("certutil")
        .args(["-A", "-n", "one-for-all-mitm", "-t", "C,,", "-i"])
        .arg(tmp.path())
        .arg("-d")
        .arg(&nss_dir)
        .status()
        .map_err(|e| RouterError::internal(format!("certutil spawn: {e}")))?;
    if !status.success() {
        return Err(RouterError::internal("certutil exited non-zero".to_owned()));
    }
    Ok(json!({"installed": true, "store": "nss", "path": nss_dir}))
}

#[cfg(target_os = "macos")]
fn install_mitm_ca(_ca_pem: &str) -> ToolResult {
    // Chrome on macOS uses the native Keychain (login.keychain-db),
    // NOT NSS. Silent install would need the user's keychain
    // password and is unsafe. We surface the exact `security` command
    // the user can run manually so behaviour is explicit, never
    // silent.
    Err(RouterError::internal(
        "net.mitm_cert.install: macOS Chrome trusts the native Keychain, \
         not NSS. Run manually:\n  \
         security add-trusted-cert -p ssl -k \"$HOME/Library/Keychains/login.keychain-db\" <ca.pem>\n\
         (requires user authentication; we will not silently inject into \
         the user keychain)"
            .to_owned(),
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn install_mitm_ca(_ca_pem: &str) -> ToolResult {
    Err(RouterError::internal(
        "net.mitm_cert.install: unsupported on this platform".to_owned(),
    ))
}
