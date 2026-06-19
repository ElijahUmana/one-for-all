//! Network interception, mocking, observation, deep-network surface.
//!
//! Implements SPEC §7 `net.*` (the v1 surface) **and** SPEC §12 U3
//! (deep-network surface) — `net.intercept.fulfill_with_body`,
//! `net.intercept.modify_request`, `net.intercept.fail`, `net.replay`,
//! `net.websocket.observe`, `net.websocket.inject_frame`,
//! `net.eventsource.observe`, `net.har.export`. Browser-scoped tools
//! (`net.proxy`, `net.mitm_cert.install`) live on [`crate::Browser`].
//!
//! ## Per-page handler state
//!
//! Handlers are per-tab (Fetch.enable is per-CDP-session, and a handler
//! registered against page A must not fire for page B). Each [`Page`]
//! owns an `Arc<PageNetworkState>` with the handler map, HAR ring
//! buffer, WS / EventSource broadcasters, and an id allocator. The
//! browser-scoped [`NetworkRegistry`] kept for backwards-compatibility
//! is now just an opaque marker — every handler-id is allocated by the
//! per-page state.
//!
//! ## N22 — Fetch.requestPaused dispatch
//!
//! [`Page::bootstrap`] subscribes the event pump to
//! `Fetch.requestPaused`. Each event is matched against the per-page
//! handler map (URL → first matching `HandlerSpec`) and dispatched:
//!
//! | Action     | CDP call                                                         |
//! |------------|------------------------------------------------------------------|
//! | `Continue` | `Fetch.continueRequest`                                          |
//! | `Modify`   | `Fetch.continueRequest` with url/method/headers/postData overrides |
//! | `Fulfill`  | `Fetch.fulfillRequest` with status/headers/body                  |
//! | `Fail`     | `Fetch.failRequest` with `errorReason`                           |
//!
//! When **no** handler matches, the pump emits `Fetch.continueRequest`
//! unmodified so the page is never broken (SPEC §12 U3 invariant).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use base64::Engine as _;
use cdp_client::generated::domains::{
    fetch as cdp_fetch, network as cdp_network, page as cdp_page, runtime as cdp_runtime,
};
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::page::Page;

// ---------------------------------------------------------------------------
// Public types — the U3 surface.
// ---------------------------------------------------------------------------

/// Action to take when a request matches an `intercept` handler.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InterceptAction {
    Continue,
    Fulfill,
    Fail,
    /// SPEC §12 U3 — `net.intercept.modify_request`. The handler's
    /// stored [`RequestOverrides`] are applied via
    /// `Fetch.continueRequest`.
    Modify,
}

/// SPEC §7 — body of a `net.mock` / `net.intercept.fulfill_with_body`
/// response. Header values are utf-8 strings; binary header bytes go
/// through the `binaryResponseHeaders` CDP path which we currently do
/// not expose.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Base64-encoded body bytes, per the CDP wire format.
    #[serde(default)]
    pub body_base64: String,
}

/// SPEC §12 U3 — overrides applied to a paused request when the
/// handler action is `Modify`. Mirrors the optional fields of
/// `Fetch.continueRequest`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestOverrides {
    /// Replacement URL (path + query). The page does not see this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Replacement HTTP method (e.g. "POST").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Replacement headers. Empty list ≡ no override.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Replacement post-data, base64-encoded per CDP wire format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_data_base64: Option<String>,
}

/// SPEC §12 U3 — failure reason for `net.intercept.fail`.
///
/// Verbatim from the CDP `Network.ErrorReason` enum. We accept any
/// string and pass it through; Chromium validates.
pub type ErrorReason = String;

/// SPEC §12 U3 — proxy config applied at next `Browser::launch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// "http" / "https" / "socks5" — anything Chromium's
    /// `--proxy-server=<scheme>://host:port` accepts.
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Optional `(user, pass)`. When set, the page-level
    /// `Fetch.enable {handleAuthRequests:true}` is engaged so a
    /// `Fetch.authRequired` handler can answer with these creds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProxyAuth>,
    /// Hosts to bypass, semicolon-joined per Chromium's
    /// `--proxy-bypass-list` syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyAuth {
    pub user: String,
    pub pass: String,
}

impl ProxyConfig {
    /// Render as a `--proxy-server=` argv element.
    pub fn to_proxy_server_arg(&self) -> String {
        format!(
            "--proxy-server={}://{}:{}",
            self.scheme, self.host, self.port
        )
    }
}

/// SPEC §12 U3 — single observed WebSocket frame, surfaced via
/// `net.websocket.observe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsFrame {
    pub request_id: String,
    pub kind: WsFrameKind,
    pub ts_ms: f64,
    /// Base64-encoded payload bytes when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_base64: Option<String>,
    /// Observed URL (`Created`-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Error message (`FrameError`-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WsFrameKind {
    Created,
    HandshakeRequest,
    HandshakeResponse,
    FrameSent,
    FrameReceived,
    FrameError,
    Closed,
}

/// SPEC §12 U3 — single observed Server-Sent-Events message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EsMessage {
    pub request_id: String,
    pub event_name: String,
    pub event_id: String,
    pub data: String,
    pub ts_ms: f64,
}

/// SPEC §12 U3 — HAR 1.2 export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarExport {
    pub log: HarLog,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarLog {
    pub version: String,
    pub creator: HarCreator,
    pub entries: Vec<HarEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarCreator {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarEntry {
    /// RFC3339 wall-clock time the request was issued.
    #[serde(rename = "startedDateTime")]
    pub started_date_time: String,
    /// Total elapsed time of the request in milliseconds.
    pub time: f64,
    pub request: HarRequest,
    pub response: HarResponse,
    pub cache: Value,
    pub timings: Value,
    /// Extension: CDP `requestId` so callers can correlate with
    /// `net.observe` / `net.replay`.
    #[serde(rename = "_requestId")]
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarRequest {
    pub method: String,
    pub url: String,
    #[serde(rename = "httpVersion")]
    pub http_version: String,
    pub cookies: Vec<Value>,
    pub headers: Vec<HarNameValue>,
    #[serde(rename = "queryString")]
    pub query_string: Vec<HarNameValue>,
    #[serde(rename = "headersSize")]
    pub headers_size: i64,
    #[serde(rename = "bodySize")]
    pub body_size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarResponse {
    pub status: i64,
    #[serde(rename = "statusText")]
    pub status_text: String,
    #[serde(rename = "httpVersion")]
    pub http_version: String,
    pub cookies: Vec<Value>,
    pub headers: Vec<HarNameValue>,
    pub content: HarContent,
    #[serde(rename = "redirectURL")]
    pub redirect_url: String,
    #[serde(rename = "headersSize")]
    pub headers_size: i64,
    #[serde(rename = "bodySize")]
    pub body_size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarContent {
    pub size: i64,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarNameValue {
    pub name: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Internal state.
// ---------------------------------------------------------------------------

/// Browser-scoped marker. Kept for back-compat with the broker that
/// already plumbs this through; per-page state lives on
/// [`PageNetworkState`].
#[derive(Default)]
pub struct NetworkRegistry {
    /// Counter shared across pages so handler ids stay globally unique
    /// even though the storage is per-page.
    #[allow(dead_code)]
    next_id: AtomicU64,
}

impl NetworkRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[allow(dead_code)]
    fn allocate_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::AcqRel) + 1;
        format!("{prefix}_{n:x}")
    }
}

/// HAR ring buffer cap. Bounded so a long-running session can't OOM
/// the broker on a single chatty page.
const HAR_RING_CAP: usize = 1024;

/// SPEC §12 U3 — per-page deep-network state. Owned via `Arc` and
/// shared with the page's event pump.
pub(crate) struct PageNetworkState {
    /// Active intercept/mock handlers, keyed by handler-id.
    handlers: Mutex<HashMap<String, HandlerSpec>>,
    /// HAR ring buffer. The pump pushes one [`HarRecord`] per
    /// `Network.requestWillBeSent` and finalises it on
    /// `responseReceived` + `loadingFinished` / `loadingFailed`.
    har: Mutex<HarRing>,
    /// Per-page allocator for `subscribe_*`.
    next_id: AtomicU64,
    /// Broadcaster for [`WsFrame`] events. Bounded; slow consumers see
    /// `Lagged` rather than blocking the pump.
    ws_tx: broadcast::Sender<WsFrame>,
    /// Broadcaster for [`EsMessage`] events.
    es_tx: broadcast::Sender<EsMessage>,
    /// Broadcaster for synthetic `net.observe` records that the pump
    /// emits when `Fetch.requestPaused` would suppress the underlying
    /// `Network.requestWillBeSent`. CR-4 net.observe coordination.
    observe_tx: broadcast::Sender<Value>,
    /// True once the WebSocket registry bootstrap has been armed for this page.
    ws_registry_armed: AtomicBool,
}

impl PageNetworkState {
    pub(crate) fn new() -> Arc<Self> {
        let (ws_tx, _) = broadcast::channel::<WsFrame>(256);
        let (es_tx, _) = broadcast::channel::<EsMessage>(256);
        let (observe_tx, _) = broadcast::channel::<Value>(256);
        Arc::new(Self {
            handlers: Mutex::new(HashMap::new()),
            har: Mutex::new(HarRing::default()),
            next_id: AtomicU64::new(0),
            ws_tx,
            es_tx,
            observe_tx,
            ws_registry_armed: AtomicBool::new(false),
        })
    }

    pub(crate) fn ws_subscribe(&self) -> broadcast::Receiver<WsFrame> {
        self.ws_tx.subscribe()
    }

    pub(crate) fn es_subscribe(&self) -> broadcast::Receiver<EsMessage> {
        self.es_tx.subscribe()
    }

    pub(crate) fn observe_subscribe(&self) -> broadcast::Receiver<Value> {
        self.observe_tx.subscribe()
    }

    fn allocate_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::AcqRel) + 1;
        format!("{prefix}_{n:x}")
    }

    /// Insert a handler. Returns its id.
    fn insert_handler(&self, mut spec: HandlerSpec) -> String {
        let id = self.allocate_id(spec.id_prefix);
        spec.id = id.clone();
        self.handlers.lock().insert(id.clone(), spec);
        id
    }

    /// Snapshot every active handler's URL pattern. Used to refresh
    /// `Fetch.enable`'s pattern set after registration.
    fn pattern_set(&self) -> Vec<String> {
        self.handlers
            .lock()
            .values()
            .map(|h| h.pattern.clone())
            .collect()
    }

    /// Find the first handler whose compiled regex matches `url`.
    /// Returns the spec by clone so the lock is released before
    /// dispatch.
    fn match_url(&self, url: &str) -> Option<HandlerSpec> {
        self.handlers
            .lock()
            .values()
            .find(|h| h.compiled.is_match(url))
            .cloned()
    }
}

#[derive(Clone)]
struct HandlerSpec {
    id: String,
    id_prefix: &'static str,
    pattern: String,
    compiled: Regex,
    action: InterceptAction,
    /// `Fulfill` payload.
    mock: Option<MockResponse>,
    /// `Modify` overrides.
    overrides: Option<RequestOverrides>,
    /// `Fail` reason.
    error_reason: Option<ErrorReason>,
}

// ---------------------------------------------------------------------------
// HAR ring buffer.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HarRing {
    /// Open records, keyed by CDP `requestId`.
    open: HashMap<String, HarRecord>,
    /// Finalised records ordered oldest → newest.
    closed: VecDeque<HarEntry>,
}

struct HarRecord {
    request_id: String,
    method: String,
    url: String,
    http_version: String,
    request_headers: Vec<HarNameValue>,
    request_body_size: i64,
    started_wall_ms: f64,
    started_mono_ms: f64,
    response_status: i64,
    response_status_text: String,
    response_headers: Vec<HarNameValue>,
    response_mime: String,
    response_size: i64,
    finished_mono_ms: Option<f64>,
}

impl HarRing {
    fn record_request(&mut self, e: &cdp_network::RequestWillBeSentEvent) {
        let rid = match e.request_id.as_str() {
            Some(s) => s.to_owned(),
            None => return,
        };
        let req = &e.request;
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_owned();
        let url = req
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let http_version = "HTTP/1.1".to_owned();
        let request_headers = req
            .get("headers")
            .and_then(Value::as_object)
            .map(|h| {
                h.iter()
                    .map(|(k, v)| HarNameValue {
                        name: k.clone(),
                        value: v.as_str().unwrap_or("").to_owned(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let request_body_size = req
            .get("postData")
            .and_then(Value::as_str)
            .map(|s| s.len() as i64)
            .unwrap_or(0);

        let wall_time = e.wall_time.as_f64().unwrap_or(0.0);
        let timestamp = e.timestamp.as_f64().unwrap_or(0.0);

        let record = HarRecord {
            request_id: rid.clone(),
            method,
            url,
            http_version,
            request_headers,
            request_body_size,
            // CDP `wallTime` is seconds since unix epoch.
            started_wall_ms: wall_time * 1000.0,
            // CDP `timestamp` is monotonic seconds.
            started_mono_ms: timestamp * 1000.0,
            response_status: 0,
            response_status_text: String::new(),
            response_headers: Vec::new(),
            response_mime: String::new(),
            response_size: 0,
            finished_mono_ms: None,
        };
        self.open.insert(rid, record);
    }

    fn record_response(&mut self, e: &cdp_network::ResponseReceivedEvent) -> Option<f64> {
        let rid = match e.request_id.as_str() {
            Some(s) => s,
            None => return None,
        };
        let mut timestamp = None;
        if let Some(rec) = self.open.get_mut(rid) {
            let resp = &e.response;
            rec.response_status = resp.get("status").and_then(Value::as_i64).unwrap_or(0);
            rec.response_status_text = resp
                .get("statusText")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            rec.response_mime = resp
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if let Some(headers) = resp.get("headers").and_then(Value::as_object) {
                rec.response_headers = headers
                    .iter()
                    .map(|(k, v)| HarNameValue {
                        name: k.clone(),
                        value: v.as_str().unwrap_or("").to_owned(),
                    })
                    .collect();
            }
            let response_mono_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
            timestamp =
                Some((rec.started_wall_ms + (response_mono_ms - rec.started_mono_ms)) / 1000.0);
        }
        timestamp
    }

    fn record_finished(&mut self, e: &cdp_network::LoadingFinishedEvent) {
        let rid = match e.request_id.as_str() {
            Some(s) => s,
            None => return,
        };
        let timestamp_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
        if let Some(mut rec) = self.open.remove(rid) {
            rec.finished_mono_ms = Some(timestamp_ms);
            rec.response_size = e.encoded_data_length as i64;
            self.push_entry(rec);
        }
    }

    fn record_failed(&mut self, e: &cdp_network::LoadingFailedEvent) {
        let rid = match e.request_id.as_str() {
            Some(s) => s,
            None => return,
        };
        let timestamp_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
        if let Some(mut rec) = self.open.remove(rid) {
            rec.finished_mono_ms = Some(timestamp_ms);
            // Stamp a synthetic response if none observed.
            if rec.response_status == 0 {
                rec.response_status = 0;
                rec.response_status_text = e.error_text.clone();
            }
            self.push_entry(rec);
        }
    }

    fn push_entry(&mut self, rec: HarRecord) {
        let time_ms = rec
            .finished_mono_ms
            .map(|f| (f - rec.started_mono_ms).max(0.0))
            .unwrap_or(0.0);
        let started_iso = wall_ms_to_rfc3339(rec.started_wall_ms);
        let query_string = parse_query_pairs(&rec.url);
        let entry = HarEntry {
            started_date_time: started_iso,
            time: time_ms,
            request: HarRequest {
                method: rec.method,
                url: rec.url,
                http_version: rec.http_version.clone(),
                cookies: Vec::new(),
                headers: rec.request_headers,
                query_string,
                headers_size: -1,
                body_size: rec.request_body_size,
            },
            response: HarResponse {
                status: rec.response_status,
                status_text: rec.response_status_text,
                http_version: rec.http_version,
                cookies: Vec::new(),
                headers: rec.response_headers,
                content: HarContent {
                    size: rec.response_size,
                    mime_type: rec.response_mime,
                },
                redirect_url: String::new(),
                headers_size: -1,
                body_size: rec.response_size,
            },
            cache: json!({}),
            timings: json!({"send": 0, "wait": time_ms, "receive": 0}),
            request_id: rec.request_id,
        };
        if self.closed.len() >= HAR_RING_CAP {
            self.closed.pop_front();
        }
        self.closed.push_back(entry);
    }

    fn snapshot_since(&self, since_wall_ms: f64) -> Vec<HarEntry> {
        self.closed
            .iter()
            .filter(|e| {
                rfc3339_to_wall_ms(&e.started_date_time)
                    .map(|w| w >= since_wall_ms)
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }
}

/// SPEC §12 U3 — convert epoch milliseconds to a **stable, no-clock**
/// RFC3339-ish string. We deliberately avoid bringing in a date crate
/// here: HAR's only requirement is that the field is parseable, and we
/// ship our own round-trip via [`rfc3339_to_wall_ms`].
fn wall_ms_to_rfc3339(wall_ms: f64) -> String {
    // ISO 8601 / RFC 3339 with millisecond precision and a synthetic
    // `Z` suffix. The HAR consumer only needs ordering.
    let ms = wall_ms as i64;
    let secs = ms / 1000;
    let frac = (ms % 1000).unsigned_abs();
    let (y, mo, d, h, mi, s) = unix_secs_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{frac:03}Z")
}

fn rfc3339_to_wall_ms(s: &str) -> Option<f64> {
    // We only parse the format we emit. Strict on shape; lenient on
    // missing milliseconds.
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let h: u32 = s.get(11..13)?.parse().ok()?;
    let mi: u32 = s.get(14..16)?.parse().ok()?;
    let sec: u32 = s.get(17..19)?.parse().ok()?;
    let ms: u32 = if bytes.get(19) == Some(&b'.') {
        s.get(20..23).and_then(|t| t.parse().ok()).unwrap_or(0)
    } else {
        0
    };
    let secs = ymdhms_to_unix_secs(y, mo, d, h, mi, sec);
    Some(secs as f64 * 1000.0 + ms as f64)
}

/// Pure-Rust unix-seconds → (Y,M,D,h,m,s) for our HAR formatter.
/// Civil-from-days algorithm by Howard Hinnant (public domain).
fn unix_secs_to_ymdhms(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let h = (seconds_of_day / 3600) as u32;
    let mi = ((seconds_of_day % 3600) / 60) as u32;
    let s = (seconds_of_day % 60) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y_civ = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y_civ + 1 } else { y_civ };
    (y, mo, d, h, mi, s)
}

fn ymdhms_to_unix_secs(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    let mo = mo as i64;
    let d = d as i64;
    let y_civ = if mo <= 2 { y - 1 } else { y };
    let era = y_civ.div_euclid(400);
    let yoe = y_civ - era * 400;
    let m = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + (h as i64) * 3600 + (mi as i64) * 60 + (s as i64)
}

fn parse_query_pairs(url: &str) -> Vec<HarNameValue> {
    let q_start = match url.find('?') {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    let q_end = url.find('#').unwrap_or(url.len());
    if q_end <= q_start {
        return Vec::new();
    }
    url[q_start..q_end]
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let mut split = kv.splitn(2, '=');
            HarNameValue {
                name: split.next().unwrap_or("").to_owned(),
                value: split.next().unwrap_or("").to_owned(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Glob → regex.
// ---------------------------------------------------------------------------

/// SPEC §7 — convert a Chromium-style URL glob pattern into a Rust
/// regex anchored on both ends. `*` matches any run of non-newline
/// characters; everything else is escaped. Same semantics as
/// `Fetch.RequestPattern.urlPattern`.
fn glob_to_regex(pattern: &str) -> Result<Regex> {
    let mut re = String::with_capacity(pattern.len() * 2 + 4);
    re.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => re.push_str(".*"),
            // Same character class regex specials, kept verbatim.
            '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' | '?' => {
                re.push('\\');
                re.push(ch);
            }
            _ => re.push(ch),
        }
    }
    re.push('$');
    Regex::new(&re).with_context(|| format!("compiling URL pattern {pattern:?}"))
}

// ---------------------------------------------------------------------------
// Public Page surface — the U3 tools.
// ---------------------------------------------------------------------------

impl Page {
    /// Implements `net.intercept` (SPEC §7 — coarse action).
    ///
    /// For richer modes use [`Self::net_intercept_fulfill_with_body`],
    /// [`Self::net_intercept_modify_request`], or
    /// [`Self::net_intercept_fail`].
    pub async fn net_intercept(
        &self,
        _registry: &NetworkRegistry,
        pattern: &str,
        action: InterceptAction,
    ) -> Result<String> {
        let compiled = glob_to_regex(pattern)?;
        let id = self.network_state().insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "h",
            pattern: pattern.to_owned(),
            compiled,
            action,
            mock: None,
            overrides: None,
            error_reason: None,
        });
        self.refresh_fetch_patterns().await?;
        Ok(id)
    }

    /// Implements `net.mock` / `net.intercept.fulfill_with_body`
    /// (SPEC §7 + §12 U3).
    pub async fn net_mock(
        &self,
        _registry: &NetworkRegistry,
        pattern: &str,
        response: MockResponse,
    ) -> Result<String> {
        self.net_intercept_fulfill_with_body(pattern, response)
            .await
    }

    /// SPEC §12 U3 — `net.intercept.fulfill_with_body`.
    pub async fn net_intercept_fulfill_with_body(
        &self,
        pattern: &str,
        response: MockResponse,
    ) -> Result<String> {
        let compiled = glob_to_regex(pattern)?;
        let id = self.network_state().insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "m",
            pattern: pattern.to_owned(),
            compiled,
            action: InterceptAction::Fulfill,
            mock: Some(response),
            overrides: None,
            error_reason: None,
        });
        self.refresh_fetch_patterns().await?;
        Ok(id)
    }

    /// SPEC §12 U3 — `net.intercept.modify_request`.
    pub async fn net_intercept_modify_request(
        &self,
        pattern: &str,
        overrides: RequestOverrides,
    ) -> Result<String> {
        let compiled = glob_to_regex(pattern)?;
        let id = self.network_state().insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "mod",
            pattern: pattern.to_owned(),
            compiled,
            action: InterceptAction::Modify,
            mock: None,
            overrides: Some(overrides),
            error_reason: None,
        });
        self.refresh_fetch_patterns().await?;
        Ok(id)
    }

    /// SPEC §12 U3 — `net.intercept.fail`.
    pub async fn net_intercept_fail(
        &self,
        pattern: &str,
        error_reason: ErrorReason,
    ) -> Result<String> {
        let compiled = glob_to_regex(pattern)?;
        let id = self.network_state().insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "fail",
            pattern: pattern.to_owned(),
            compiled,
            action: InterceptAction::Fail,
            mock: None,
            overrides: None,
            error_reason: Some(error_reason),
        });
        self.refresh_fetch_patterns().await?;
        Ok(id)
    }

    /// Implements `net.observe` (SPEC §7 + §12 U3 CR-4 coordination).
    ///
    /// Subscribers receive synthetic JSON records emitted by the page
    /// pump: `{"kind":"request_will_be_sent",...}` or
    /// `{"kind":"response_received",...}`. The pump emits these for
    /// **every** request — whether `Fetch.requestPaused` intercepted it
    /// or not — so observers see the same view regardless of whether
    /// `Fetch.enable` is active.
    pub fn allocate_subscription_id(&self, prefix: &str) -> String {
        self.network_state().allocate_id(prefix)
    }

    pub fn net_observe_subscription_id(
        &self,
        _registry: &NetworkRegistry,
        _filter: Option<&str>,
    ) -> String {
        self.allocate_subscription_id("s")
    }

    /// SPEC §12 U3 — receiver for `net.observe`. Each frame is a JSON
    /// blob suitable for direct relay.
    pub fn net_observe_subscribe(&self) -> broadcast::Receiver<Value> {
        self.network_state().observe_subscribe()
    }

    /// SPEC §12 U3 — `net.replay` via `Network.replayXHR`.
    pub async fn net_replay(&self, request_id: &str) -> Result<()> {
        self.cdp_send(cdp_network::ReplayXhrParams {
            request_id: Value::String(request_id.to_owned()),
        })
        .await
        .context("Network.replayXHR")?;
        Ok(())
    }

    /// SPEC §12 U3 — `net.websocket.observe` subscription receiver.
    pub fn net_websocket_observe(&self) -> broadcast::Receiver<WsFrame> {
        self.network_state().ws_subscribe()
    }

    pub(crate) async fn ensure_ws_registry_shim(&self) -> Result<()> {
        if self
            .network_state()
            .ws_registry_armed
            .load(Ordering::Acquire)
        {
            return Ok(());
        }
        const WS_REGISTRY_BOOTSTRAP: &str = r#"(() => {
            if (globalThis.__claudeBridgeWSInstalled) return true;
            const registry = Array.isArray(globalThis.__claudeBridgeWS)
                ? globalThis.__claudeBridgeWS
                : [];
            Object.defineProperty(globalThis, '__claudeBridgeWS', {
                configurable: true,
                enumerable: false,
                writable: true,
                value: registry,
            });
            const OriginalWebSocket = globalThis.WebSocket;
            if (typeof OriginalWebSocket !== 'function') {
                return false;
            }
            class OneForAllWebSocket extends OriginalWebSocket {
                constructor(...args) {
                    super(...args);
                    const reg = globalThis.__claudeBridgeWS;
                    if (Array.isArray(reg) && !reg.includes(this)) {
                        reg.push(this);
                    }
                    const drop = () => {
                        const regNow = globalThis.__claudeBridgeWS;
                        if (!Array.isArray(regNow)) return;
                        const idx = regNow.indexOf(this);
                        if (idx >= 0) regNow.splice(idx, 1);
                    };
                    this.addEventListener('close', drop, { once: true });
                    this.addEventListener('error', () => {
                        if (this.readyState === OriginalWebSocket.CLOSED) drop();
                    });
                }
            }
            Object.defineProperty(OneForAllWebSocket, 'name', { value: 'WebSocket' });
            globalThis.WebSocket = OneForAllWebSocket;
            globalThis.__claudeBridgeWSInstalled = true;
            return true;
        })();"#;

        self.cdp_send(cdp_page::AddScriptToEvaluateOnNewDocumentParams {
            source: WS_REGISTRY_BOOTSTRAP.to_owned(),
            ..Default::default()
        })
        .await
        .context("Page.addScriptToEvaluateOnNewDocument (ws registry)")?;

        let res = self
            .cdp_send(cdp_runtime::EvaluateParams {
                expression: WS_REGISTRY_BOOTSTRAP.to_owned(),
                return_by_value: Some(true),
                await_promise: Some(false),
                ..Default::default()
            })
            .await
            .context("Runtime.evaluate (ws registry)")?;
        if res
            .result
            .get("value")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.network_state()
                .ws_registry_armed
                .store(true, Ordering::Release);
            Ok(())
        } else {
            Err(anyhow!("websocket registry bootstrap returned false"))
        }
    }

    /// SPEC §12 U3 — `net.websocket.inject_frame`.
    ///
    /// **Limitation:** CDP has no first-class "send WS frame" command.
    /// We fall back to `Runtime.evaluate` against a JS shim that walks
    /// `window.__claudeBridgeWS`.
    pub async fn net_websocket_inject_frame(
        &self,
        url_substring: &str,
        payload_base64: &str,
    ) -> Result<()> {
        self.ensure_ws_registry_shim().await?;
        // Caller passes base64; decode and re-encode safely as a JS
        // string literal (only ASCII bytes, so direct quoting works).
        let payload = base64::engine::general_purpose::STANDARD
            .decode(payload_base64)
            .context("payload_base64 is not valid base64")?;
        let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "[]".to_owned());
        let url_json = serde_json::to_string(url_substring).unwrap_or_else(|_| "\"\"".to_owned());
        let expr = format!(
            r#"(() => {{
                const reg = (window.__claudeBridgeWS || []);
                const target = reg.find(w => w && w.url && w.url.indexOf({url_json}) >= 0);
                if (!target) return {{ ok: false, reason: "no-matching-ws" }};
                if (target.readyState !== WebSocket.OPEN) {{
                    return {{ ok: false, reason: `ws-not-open:${{target.readyState}}` }};
                }}
                const bytes = new Uint8Array({payload_json});
                target.send(bytes.buffer);
                return {{ ok: true }};
            }})()"#
        );
        let res = self
            .cdp_send(cdp_runtime::EvaluateParams {
                expression: expr,
                return_by_value: Some(true),
                await_promise: Some(false),
                ..Default::default()
            })
            .await
            .context("Runtime.evaluate (ws inject)")?;
        let value = res.result.get("value").cloned().unwrap_or(Value::Null);
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("ws-shim-not-armed");
            Err(anyhow!("net.websocket.inject_frame failed: {reason}"))
        }
    }

    /// SPEC §12 U3 — `net.eventsource.observe` subscription receiver.
    pub fn net_eventsource_observe(&self) -> broadcast::Receiver<EsMessage> {
        self.network_state().es_subscribe()
    }

    /// SPEC §12 U3 — `net.har.export {tab_id, since_ts}`.
    /// `since_wall_ms` is wall-clock epoch milliseconds; `0` returns
    /// every retained entry.
    pub fn net_har_export(&self, since_wall_ms: f64) -> HarExport {
        let entries = self
            .network_state()
            .har
            .lock()
            .snapshot_since(since_wall_ms);
        HarExport {
            log: HarLog {
                version: "1.2".to_owned(),
                creator: HarCreator {
                    name: "one-for-all".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                entries,
            },
        }
    }

    /// Re-emit `Fetch.enable` with the union of every active handler's
    /// pattern. Idempotent.
    async fn refresh_fetch_patterns(&self) -> Result<()> {
        let patterns: Vec<Value> = self
            .network_state()
            .pattern_set()
            .into_iter()
            .map(|p| json!({"urlPattern": p, "requestStage": "Request"}))
            .collect();
        let handle_auth_requests = self
            .browser()
            .proxy_config()
            .as_ref()
            .and_then(|cfg| cfg.auth.as_ref())
            .is_some();
        self.cdp_send(cdp_fetch::EnableParams {
            patterns: Some(Value::Array(patterns)),
            handle_auth_requests: Some(handle_auth_requests),
            ..Default::default()
        })
        .await
        .context("Fetch.enable")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pump-side dispatch — used by `Page::event_pump`.
// ---------------------------------------------------------------------------

/// Outcome of a Fetch.requestPaused dispatch — the pump uses this to
/// decide which CDP call to issue. Kept separate so the dispatch logic
/// is testable without a live Page.
pub(crate) enum FetchDispatch {
    Continue {
        request_id: Value,
        url: Option<String>,
        method: Option<String>,
        post_data_b64: Option<String>,
        headers: Option<Value>,
    },
    Fulfill {
        request_id: Value,
        status: u16,
        headers: Option<Value>,
        body_b64: String,
    },
    Fail {
        request_id: Value,
        error_reason: String,
    },
}

/// Pure dispatch function — public to the crate so the event pump in
/// [`crate::page`] can call it without touching `HandlerSpec`
/// internals. Returns `Continue` (no overrides) when no handler
/// matches, satisfying the SPEC §12 U3 "never break the page"
/// invariant.
pub(crate) fn dispatch_fetch_event(
    state: &PageNetworkState,
    e: &cdp_fetch::RequestPausedEvent,
) -> FetchDispatch {
    let request_id = e.request_id.clone();
    let url = e
        .request
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let matched = state.match_url(&url);
    match matched {
        None => FetchDispatch::Continue {
            request_id,
            url: None,
            method: None,
            post_data_b64: None,
            headers: None,
        },
        Some(spec) => match spec.action {
            InterceptAction::Continue => FetchDispatch::Continue {
                request_id,
                url: None,
                method: None,
                post_data_b64: None,
                headers: None,
            },
            InterceptAction::Modify => {
                let ov = spec.overrides.unwrap_or_default();
                FetchDispatch::Continue {
                    request_id,
                    url: ov.url,
                    method: ov.method,
                    post_data_b64: ov.post_data_base64,
                    headers: if ov.headers.is_empty() {
                        None
                    } else {
                        Some(Value::Array(
                            ov.headers
                                .into_iter()
                                .map(|(n, v)| json!({"name": n, "value": v}))
                                .collect(),
                        ))
                    },
                }
            }
            InterceptAction::Fulfill => {
                let mock = spec.mock.unwrap_or_default();
                let headers = if mock.headers.is_empty() {
                    None
                } else {
                    Some(Value::Array(
                        mock.headers
                            .into_iter()
                            .map(|(n, v)| json!({"name": n, "value": v}))
                            .collect(),
                    ))
                };
                FetchDispatch::Fulfill {
                    request_id,
                    status: mock.status,
                    headers,
                    body_b64: mock.body_base64,
                }
            }
            InterceptAction::Fail => FetchDispatch::Fail {
                request_id,
                error_reason: spec.error_reason.unwrap_or_else(|| "Failed".to_owned()),
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Pump-side observation — exposed so [`crate::page::event_pump`] can
// push records into the per-page broadcasters.
// ---------------------------------------------------------------------------

impl PageNetworkState {
    pub(crate) fn record_request(&self, e: &cdp_network::RequestWillBeSentEvent) {
        self.har.lock().record_request(e);
        let payload = json!({
            "kind": "request_will_be_sent",
            "request_id": e.request_id,
            "url": e.request.get("url").cloned().unwrap_or(Value::Null),
            "method": e.request.get("method").cloned().unwrap_or(Value::Null),
            "ts_ms": e.timestamp.as_f64().unwrap_or(0.0) * 1000.0,
            "timestamp": e.wall_time.as_f64().unwrap_or(0.0),
        });
        let _ = self.observe_tx.send(payload);
    }

    pub(crate) fn record_response(&self, e: &cdp_network::ResponseReceivedEvent) {
        let response_timestamp = self.har.lock().record_response(e);
        let payload = json!({
            "kind": "response_received",
            "request_id": e.request_id,
            "status": e.response.get("status").cloned().unwrap_or(Value::Null),
            "url": e.response.get("url").cloned().unwrap_or(Value::Null),
            "headers": e.response.get("headers").cloned().unwrap_or_else(|| json!({})),
            "mime_type": e.response.get("mimeType").cloned().unwrap_or(Value::Null),
            "ts_ms": e.timestamp.as_f64().unwrap_or(0.0) * 1000.0,
            "timestamp": response_timestamp.unwrap_or_else(|| e.timestamp.as_f64().unwrap_or(0.0)),
        });
        let _ = self.observe_tx.send(payload);
    }

    pub(crate) fn record_finished(&self, e: &cdp_network::LoadingFinishedEvent) {
        self.har.lock().record_finished(e);
    }

    pub(crate) fn record_failed(&self, e: &cdp_network::LoadingFailedEvent) {
        self.har.lock().record_failed(e);
    }

    pub(crate) fn record_synthetic_request(&self, e: &cdp_fetch::RequestPausedEvent) {
        // CR-4: when Fetch supersedes Network for a URL, emit a
        // synthetic record so observers see the request once and only
        // once.
        let url = e.request.get("url").cloned().unwrap_or(Value::Null);
        let method = e.request.get("method").cloned().unwrap_or(Value::Null);
        let now_secs = current_unix_seconds();
        let payload = json!({
            "kind": "request_will_be_sent",
            "synthetic": true,
            "request_id": e.request_id,
            "url": url,
            "method": method,
            "ts_ms": now_secs * 1000.0,
            "timestamp": now_secs,
        });
        let _ = self.observe_tx.send(payload);
    }

    pub(crate) fn record_ws(&self, frame: WsFrame) {
        let _ = self.ws_tx.send(frame);
    }

    pub(crate) fn record_es(&self, msg: EsMessage) {
        let _ = self.es_tx.send(msg);
    }
}

fn current_unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Tests — pure logic only; full E2E tests live under `tests/`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_to_regex_matches_wildcards() {
        let r = glob_to_regex("**/api/*").expect("compile");
        assert!(r.is_match("https://x/api/foo"));
        assert!(r.is_match("/api/bar"));
        assert!(!r.is_match("/static/img.png"));
    }

    #[test]
    fn glob_to_regex_escapes_specials() {
        let r = glob_to_regex("a.b?c").expect("compile");
        assert!(r.is_match("a.b?c"));
        assert!(!r.is_match("aXb?c"));
    }

    #[test]
    fn parse_query_pairs_handles_fragment() {
        let pairs = parse_query_pairs("/p?a=1&b=2#frag");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].name, "a");
        assert_eq!(pairs[1].value, "2");
    }

    #[test]
    fn rfc3339_round_trips() {
        let s = wall_ms_to_rfc3339(1_700_000_000_000.0);
        let back = rfc3339_to_wall_ms(&s).expect("parse");
        assert!((back - 1_700_000_000_000.0).abs() < 1.0);
    }

    #[test]
    fn dispatch_no_match_returns_continue_unmodified() {
        let st = PageNetworkState::new();
        let e = cdp_fetch::RequestPausedEvent {
            request_id: Value::String("rq1".to_owned()),
            request: json!({"url": "https://example.com/no-handler"}),
            frame_id: Value::Null,
            resource_type: Value::Null,
            response_error_reason: None,
            response_status_code: None,
            response_status_text: None,
            response_headers: None,
            network_id: None,
            redirected_request_id: None,
        };
        match dispatch_fetch_event(&st, &e) {
            FetchDispatch::Continue {
                url,
                method,
                post_data_b64,
                headers,
                ..
            } => {
                assert!(url.is_none());
                assert!(method.is_none());
                assert!(post_data_b64.is_none());
                assert!(headers.is_none());
            }
            _ => panic!("expected Continue with no overrides"),
        }
    }

    #[test]
    fn dispatch_fulfill_uses_mock() {
        let st = PageNetworkState::new();
        let _id = st.insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "m",
            pattern: "**/api/*".to_owned(),
            compiled: glob_to_regex("**/api/*").expect("compile"),
            action: InterceptAction::Fulfill,
            mock: Some(MockResponse {
                status: 418,
                headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
                body_base64: "dGVhcG90".to_owned(),
            }),
            overrides: None,
            error_reason: None,
        });
        let e = cdp_fetch::RequestPausedEvent {
            request_id: Value::String("rq1".to_owned()),
            request: json!({"url": "https://x/api/foo"}),
            frame_id: Value::Null,
            resource_type: Value::Null,
            response_error_reason: None,
            response_status_code: None,
            response_status_text: None,
            response_headers: None,
            network_id: None,
            redirected_request_id: None,
        };
        match dispatch_fetch_event(&st, &e) {
            FetchDispatch::Fulfill {
                status, body_b64, ..
            } => {
                assert_eq!(status, 418);
                assert_eq!(body_b64, "dGVhcG90");
            }
            _ => panic!("expected Fulfill"),
        }
    }

    #[test]
    fn dispatch_modify_carries_overrides() {
        let st = PageNetworkState::new();
        let _id = st.insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "mod",
            pattern: "**/x?**".to_owned(),
            compiled: glob_to_regex("**/x?**").expect("compile"),
            action: InterceptAction::Modify,
            mock: None,
            overrides: Some(RequestOverrides {
                url: Some("/x?modified=1".to_owned()),
                method: None,
                headers: vec![],
                post_data_base64: None,
            }),
            error_reason: None,
        });
        let e = cdp_fetch::RequestPausedEvent {
            request_id: Value::String("rq2".to_owned()),
            request: json!({"url": "https://h/x?orig=1"}),
            frame_id: Value::Null,
            resource_type: Value::Null,
            response_error_reason: None,
            response_status_code: None,
            response_status_text: None,
            response_headers: None,
            network_id: None,
            redirected_request_id: None,
        };
        match dispatch_fetch_event(&st, &e) {
            FetchDispatch::Continue { url, .. } => {
                assert_eq!(url.as_deref(), Some("/x?modified=1"));
            }
            _ => panic!("expected Continue with overrides"),
        }
    }

    #[test]
    fn dispatch_fail_carries_reason() {
        let st = PageNetworkState::new();
        let _id = st.insert_handler(HandlerSpec {
            id: String::new(),
            id_prefix: "fail",
            pattern: "**/dead".to_owned(),
            compiled: glob_to_regex("**/dead").expect("compile"),
            action: InterceptAction::Fail,
            mock: None,
            overrides: None,
            error_reason: Some("NameNotResolved".to_owned()),
        });
        let e = cdp_fetch::RequestPausedEvent {
            request_id: Value::String("rq3".to_owned()),
            request: json!({"url": "https://gone.example/dead"}),
            frame_id: Value::Null,
            resource_type: Value::Null,
            response_error_reason: None,
            response_status_code: None,
            response_status_text: None,
            response_headers: None,
            network_id: None,
            redirected_request_id: None,
        };
        match dispatch_fetch_event(&st, &e) {
            FetchDispatch::Fail { error_reason, .. } => assert_eq!(error_reason, "NameNotResolved"),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn observe_payloads_include_protocol_fields() {
        let state = PageNetworkState::new();
        let mut rx = state.observe_subscribe();
        state.record_request(&cdp_network::RequestWillBeSentEvent {
            request_id: Value::String("R1".to_owned()),
            loader_id: Value::Null,
            document_url: String::new(),
            request: json!({
                "method": "GET",
                "url": "https://example.com/a",
                "headers": {},
            }),
            timestamp: json!(100.0),
            wall_time: json!(1_700_000_123.456),
            initiator: Value::Null,
            redirect_has_extra_info: false,
            redirect_response: None,
            r#type: None,
            frame_id: None,
            has_user_gesture: None,
            render_blocking_behavior: None,
        });
        let req = rx.try_recv().expect("request payload");
        assert_eq!(
            req.get("timestamp").and_then(Value::as_f64),
            Some(1_700_000_123.456)
        );
        assert_eq!(req.get("method").and_then(Value::as_str), Some("GET"));

        state.record_response(&cdp_network::ResponseReceivedEvent {
            request_id: Value::String("R1".to_owned()),
            loader_id: Value::Null,
            timestamp: json!(100.5),
            r#type: Value::Null,
            response: json!({
                "status": 200,
                "url": "https://example.com/a",
                "headers": {"Content-Type": "text/plain"},
                "mimeType": "text/plain",
            }),
            has_extra_info: false,
            frame_id: None,
        });
        let resp = rx.try_recv().expect("response payload");
        assert_eq!(
            resp.get("mime_type").and_then(Value::as_str),
            Some("text/plain")
        );
        assert!(resp.get("headers").and_then(Value::as_object).is_some());
    }
    #[test]
    fn har_round_trip_one_request() {
        let mut ring = HarRing::default();
        let req = cdp_network::RequestWillBeSentEvent {
            request_id: Value::String("R1".to_owned()),
            loader_id: Value::Null,
            document_url: String::new(),
            request: json!({
                "method": "GET",
                "url": "https://h/p?a=1",
                "headers": {"Accept": "*/*"},
            }),
            timestamp: json!(100.0),
            wall_time: json!(1_700_000_000.0),
            initiator: Value::Null,
            redirect_has_extra_info: false,
            redirect_response: None,
            r#type: None,
            frame_id: None,
            has_user_gesture: None,
            render_blocking_behavior: None,
        };
        ring.record_request(&req);
        let resp = cdp_network::ResponseReceivedEvent {
            request_id: Value::String("R1".to_owned()),
            loader_id: Value::Null,
            timestamp: json!(100.5),
            r#type: Value::Null,
            response: json!({
                "status": 200,
                "statusText": "OK",
                "headers": {"Server": "test"},
                "mimeType": "text/plain",
            }),
            has_extra_info: false,
            frame_id: None,
        };
        ring.record_response(&resp);
        let fin = cdp_network::LoadingFinishedEvent {
            request_id: Value::String("R1".to_owned()),
            timestamp: json!(101.0),
            encoded_data_length: 42.0,
        };
        ring.record_finished(&fin);
        let entries = ring.snapshot_since(0.0);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.request.url, "https://h/p?a=1");
        assert_eq!(e.response.status, 200);
        assert_eq!(e.response.content.size, 42);
        assert_eq!(e.request.query_string.len(), 1);
    }

    #[test]
    fn proxy_arg_renders() {
        let p = ProxyConfig {
            scheme: "http".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: 9000,
            auth: None,
            bypass: None,
        };
        assert_eq!(
            p.to_proxy_server_arg(),
            "--proxy-server=http://127.0.0.1:9000"
        );
    }
}
