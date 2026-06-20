//! Typed input schemas for every tool exposed by the MCP server.
//!
//! Locked against SPEC §7. Every struct uses `#[derive(Deserialize, JsonSchema)]`
//! so the same definition drives runtime parsing of `tools/call` arguments AND
//! the `inputSchema` reported by `tools/list`.
//!
//! Tools are dispatched to the broker by their canonical method name (e.g.
//! `tab.open`) per SPEC §2 — the broker exposes them directly, NOT wrapped in
//! a `tool.call` envelope.

#![allow(clippy::module_name_repetitions, dead_code)]
// Fields exist for Deserialize parsing + schemars JsonSchema generation; the
// MCP server forwards `args` as opaque JSON to the broker, so structural
// fields aren't read directly in Rust. Validation happens via the
// Deserialize-into-discard pattern in `tools.rs`.

use schemars::JsonSchema;
use serde::Deserialize;

// ---------------------------------------------------------------------
// browser.context.* (SPEC §7)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserContextCreate {
    /// Optional human-readable label (shown in `ofa-status`).
    #[serde(default)]
    pub label: Option<String>,
    /// If true (default), tabs persist across session exits via the
    /// per-session `--user-data-dir`.
    #[serde(default)]
    pub persist: Option<bool>,
    /// SPEC §10 M3 — accepted for forward compatibility. In v1 the default
    /// context is created at session launch, so passing `stealth:false` here
    /// does not reconfigure already-launched pages.
    #[serde(default)]
    pub stealth: Option<bool>,
    /// SPEC §10 M10 — enable structured trace recording for the bound session.
    #[serde(default)]
    pub trace: Option<bool>,
    /// SPEC §11 V4 — vision pipeline mode. `off` (default), `on_demand`
    /// (lazy-build the pipeline on first `vision.*` call; no continuous
    /// capture), or `continuous` (start CDP screencast on every tab and
    /// stream `event/notify {topic: "vision.frame"}` events to the MCP
    /// client).
    #[serde(default)]
    pub vision: Option<String>,
    /// SPEC §11 V4 — peak frames per second when an action is in flight.
    /// 1..=60. Default 30 when `vision = continuous`. Ignored otherwise.
    #[serde(default)]
    pub fps: Option<u32>,
    /// SPEC §11 V4 — idle frames per second (no action in flight).
    /// Default 5. Ignored when `vision != continuous`.
    #[serde(default)]
    pub idle_fps: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserContextList {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserContextDestroy {
    pub context_id: String,
}

// ---------------------------------------------------------------------
// tab.* (SPEC §7)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitUntil {
    Load,
    Domcontentloaded,
    Networkidle,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabOpen {
    pub url: String,
    #[serde(default)]
    pub wait_until: Option<WaitUntil>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabList {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabClose {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabFocus {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabNavigate {
    pub tab_id: String,
    pub url: String,
    #[serde(default)]
    pub wait_until: Option<WaitUntil>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Per SPEC §7, `predicate` is one of:
///   "load" | "networkidle" | {"selector": "..."} | {"url_regex": "..."}
/// Modeled here as an untagged enum with a permissive `serde_json::Value`
/// fallback so any future shape variants pass through unchanged.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TabWaitPredicate {
    Named(String), // "load" | "networkidle"
    Object(serde_json::Value),
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabWait {
    pub tab_id: String,
    pub predicate: TabWaitPredicate,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

// ---------------------------------------------------------------------
// page.* (SPEC §7)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageSnapshot {
    pub tab_id: String,
    /// SPEC §10 M2 — when present, `page.snapshot` returns only mutations
    /// since the given seq (a previous response's `snapshot_seq`). Falls
    /// back to a full snapshot when behind the anchor, on log overflow,
    /// or after a top-frame navigation. See SPEC §7 "Snapshot delta shape".
    #[serde(default)]
    pub since_seq: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageScreenshot {
    pub tab_id: String,
    #[serde(default)]
    pub format: Option<ImageFormat>,
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub capture_beyond_viewport: Option<bool>,
    /// Optional element ref (clipped to bbox).
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageReadText {
    pub tab_id: String,
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageClick {
    pub tab_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    #[serde(default)]
    pub button: Option<MouseButton>,
    #[serde(default)]
    pub click_count: Option<u8>,
    #[serde(default)]
    pub realistic: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageType {
    pub tab_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub text: String,
    #[serde(default)]
    pub delay_ms: Option<u32>,
    #[serde(default)]
    pub clear_first: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageDirection {
    Forward,
    Backward,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageScrollEasing {
    Linear,
    EaseOut,
    EaseInOut,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageTouchTap {
    pub tab_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub tap_count: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageTouchSwipe {
    pub tab_id: String,
    pub start_x: f64,
    pub start_y: f64,
    pub end_x: f64,
    pub end_y: f64,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageTouchPinch {
    pub tab_id: String,
    pub center_x: f64,
    pub center_y: f64,
    pub start_radius: f64,
    pub end_radius: f64,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageTouchRotate {
    pub tab_id: String,
    pub center_x: f64,
    pub center_y: f64,
    pub radius: f64,
    pub angle_deg: f64,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePointerPress {
    pub tab_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub button: Option<MouseButton>,
    #[serde(default)]
    pub click_count: Option<u32>,
    #[serde(default)]
    pub pressure: Option<f64>,
    #[serde(default)]
    pub tangential_pressure: Option<f64>,
    #[serde(default)]
    pub tilt_x: Option<f64>,
    #[serde(default)]
    pub tilt_y: Option<f64>,
    #[serde(default)]
    pub twist: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePointerMove {
    pub tab_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub buttons: Option<i64>,
    #[serde(default)]
    pub pressure: Option<f64>,
    #[serde(default)]
    pub tangential_pressure: Option<f64>,
    #[serde(default)]
    pub tilt_x: Option<f64>,
    #[serde(default)]
    pub tilt_y: Option<f64>,
    #[serde(default)]
    pub twist: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePointerRelease {
    pub tab_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub button: Option<MouseButton>,
    #[serde(default)]
    pub click_count: Option<u32>,
    #[serde(default)]
    pub pressure: Option<f64>,
    #[serde(default)]
    pub tangential_pressure: Option<f64>,
    #[serde(default)]
    pub tilt_x: Option<f64>,
    #[serde(default)]
    pub tilt_y: Option<f64>,
    #[serde(default)]
    pub twist: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageGesturePinch {
    pub tab_id: String,
    pub center_x: f64,
    pub center_y: f64,
    pub start_radius: f64,
    pub scale_factor: f64,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageGestureRotate {
    pub tab_id: String,
    pub center_x: f64,
    pub center_y: f64,
    pub radius: f64,
    pub angle_deg: f64,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageGestureLongpress {
    pub tab_id: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageDragFileDrop {
    pub tab_id: String,
    #[serde(rename = "target_ref")]
    pub target_ref: String,
    pub file_paths: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageKeyboardShortcut {
    pub tab_id: String,
    pub accel: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageKeyboardIme {
    pub tab_id: String,
    pub composition_string: String,
    #[serde(default)]
    pub commit: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageDeadKey {
    pub tab_id: String,
    pub accent: String,
    pub base: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageScrollPrecise {
    pub tab_id: String,
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
    pub dx: f64,
    pub dy: f64,
    #[serde(default)]
    pub momentum: Option<bool>,
    #[serde(default)]
    pub easing: Option<PageScrollEasing>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageTabTraversal {
    pub tab_id: String,
    pub direction: PageDirection,
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageRightClickMenuNavigate {
    pub tab_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub item_path: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageKeypress {
    pub tab_id: String,
    /// Single key name (e.g. "Enter", "Tab", "ArrowDown").
    pub key: String,
    /// Modifiers from {"Alt","Control","Meta","Shift"}.
    #[serde(default)]
    pub modifiers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageScroll {
    pub tab_id: String,
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
    pub dx: f64,
    pub dy: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageHover {
    pub tab_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageDrag {
    pub tab_id: String,
    pub from_ref: String,
    pub to_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageEval {
    pub tab_id: String,
    pub expression: String,
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
    #[serde(default)]
    pub return_by_value: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CookieAction {
    Get,
    Set,
    Clear,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCookies {
    pub tab_id: String,
    pub action: CookieAction,
    /// Required for `set`; otherwise ignored.
    #[serde(default)]
    pub cookies: Option<serde_json::Value>,
    /// Optional exact-domain filter for `clear`.
    #[serde(default)]
    pub domain: Option<String>,
    /// Optional exact-name filter for `clear`.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Local,
    Session,
    Indexeddb,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorage {
    pub tab_id: String,
    pub kind: StorageKind,
    pub action: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorageGet {
    pub tab_id: String,
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorageSet {
    pub tab_id: String,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorageDelete {
    pub tab_id: String,
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorageClear {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorageCas {
    pub tab_id: String,
    pub key: String,
    #[serde(default)]
    pub expected: Option<String>,
    pub value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CookiePartitionKey {
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCookieDeepSetCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub secure: Option<bool>,
    #[serde(default, rename = "http_only")]
    pub http_only: Option<bool>,
    #[serde(default)]
    pub same_site: Option<String>,
    #[serde(default)]
    pub expires: Option<f64>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub source_scheme: Option<String>,
    #[serde(default)]
    pub source_port: Option<i64>,
    #[serde(default)]
    pub partition_key: Option<CookiePartitionKey>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCookiesDeepSet {
    pub tab_id: String,
    pub cookie: PageCookieDeepSetCookie,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageStorageQuota {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePermissionsQuery {
    pub tab_id: String,
    pub permission: serde_json::Value,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub embedded_origin: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePermissionsGrant {
    pub tab_id: String,
    pub permission: serde_json::Value,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub embedded_origin: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePermissionsRevoke {
    pub tab_id: String,
    #[serde(default)]
    pub permission: Option<serde_json::Value>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub embedded_origin: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageIndexeddbListDatabases {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageIndexeddbListStores {
    pub tab_id: String,
    pub database_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageIndexeddbQuery {
    pub tab_id: String,
    pub database_name: String,
    pub object_store_name: String,
    #[serde(default)]
    pub index_name: Option<String>,
    #[serde(default)]
    pub skip_count: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub key_range: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageIndexeddbPut {
    pub tab_id: String,
    pub database_name: String,
    pub object_store_name: String,
    pub key: serde_json::Value,
    pub value: serde_json::Value,
    #[serde(default)]
    pub database_version: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageIndexeddbDelete {
    pub tab_id: String,
    pub database_name: String,
    pub object_store_name: String,
    pub key_range: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageIndexeddbDeleteDatabase {
    pub tab_id: String,
    pub database_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCacheApiList {
    pub tab_id: String,
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCacheApiInspect {
    pub tab_id: String,
    pub cache_id: String,
    #[serde(default)]
    pub request_url: Option<String>,
    #[serde(default)]
    pub request_headers: Option<serde_json::Value>,
    #[serde(default)]
    pub skip_count: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub path_filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCacheApiDelete {
    pub tab_id: String,
    pub cache_id: String,
    #[serde(default)]
    pub request_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageViewport {
    pub tab_id: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub device_scale_factor: Option<f64>,
    #[serde(default)]
    pub mobile: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageUserAgent {
    pub tab_id: String,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub accept_language: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageGeo {
    pub tab_id: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageDarkMode {
    pub tab_id: String,
    pub enabled: bool,
}

// ---------------------------------------------------------------------
// page.network_conditions / page.emulate (SPEC §10 M7+M8)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageNetworkConditions {
    pub tab_id: String,
    /// Force offline.
    #[serde(default)]
    pub offline: Option<bool>,
    /// Round-trip latency in milliseconds.
    #[serde(default)]
    pub latency_ms: Option<u32>,
    /// Download bandwidth in bytes/sec.
    #[serde(default)]
    pub download_bps: Option<u64>,
    /// Upload bandwidth in bytes/sec.
    #[serde(default)]
    pub upload_bps: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageEmulate {
    pub tab_id: String,
    /// BCP-47 locale tag, e.g. "en-US" or "ja-JP".
    #[serde(default)]
    pub locale: Option<String>,
    /// IANA timezone name, e.g. "America/New_York".
    #[serde(default)]
    pub timezone: Option<String>,
    /// CPU throttling rate (1.0 = no throttling, 4.0 = 4× slower).
    #[serde(default)]
    pub cpu_throttle: Option<f64>,
}

// ---------------------------------------------------------------------
// page.* — SPEC §12 U4 (perf + introspection) + U5 (PDF + print)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePerformanceTimelineStart {
    pub tab_id: String,
    /// Comma-separated CDP categories. Empty/omitted uses the DevTools
    /// "Performance" preset.
    #[serde(default)]
    pub categories: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePerformanceTimelineStop {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePerformanceMetrics {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCoverageJsStart {
    pub tab_id: String,
    /// Collect accurate call counts (default true).
    #[serde(default)]
    pub call_count: Option<bool>,
    /// Collect block-based coverage (default false).
    #[serde(default)]
    pub detailed: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCoverageJsTake {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCoverageCssStart {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCoverageCssTake {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageHeapSnapshot {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageHeapSampleAlloc {
    pub tab_id: String,
    /// Sample window in milliseconds.
    pub duration_ms: u64,
    /// Average sample interval in bytes (default 32_768).
    #[serde(default)]
    pub sampling_interval_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageCpuProfile {
    pub tab_id: String,
    /// Profile window in milliseconds.
    pub duration_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageLayoutMetrics {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePaintFlash {
    pub tab_id: String,
    pub enable: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePdf {
    pub tab_id: String,
    #[serde(default)]
    pub landscape: Option<bool>,
    #[serde(default)]
    pub display_header_footer: Option<bool>,
    #[serde(default)]
    pub print_background: Option<bool>,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub paper_width: Option<f64>,
    #[serde(default)]
    pub paper_height: Option<f64>,
    #[serde(default)]
    pub margin_top: Option<f64>,
    #[serde(default)]
    pub margin_bottom: Option<f64>,
    #[serde(default)]
    pub margin_left: Option<f64>,
    #[serde(default)]
    pub margin_right: Option<f64>,
    #[serde(default)]
    pub page_ranges: Option<String>,
    #[serde(default)]
    pub header_template: Option<String>,
    #[serde(default)]
    pub footer_template: Option<String>,
    #[serde(default)]
    pub prefer_css_page_size: Option<bool>,
    #[serde(default)]
    pub generate_tagged_pdf: Option<bool>,
    #[serde(default)]
    pub generate_document_outline: Option<bool>,
    /// Force stream-mode delivery (always saves to disk).
    #[serde(default)]
    pub force_stream: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PagePrintPreview {
    pub tab_id: String,
    /// Image format: "png" (default) or "jpeg".
    #[serde(default)]
    pub format: Option<String>,
    /// Capture beyond the viewport (full-page-ish). Default false.
    #[serde(default)]
    pub capture_beyond_viewport: Option<bool>,
}

// ---------------------------------------------------------------------
// net.* (SPEC §7)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterceptAction {
    Continue,
    Fulfill,
    Fail,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetIntercept {
    pub tab_id: String,
    pub pattern: String,
    pub action: InterceptAction,
    #[serde(default)]
    pub overrides: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetMockResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub body_base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetMock {
    pub tab_id: String,
    pub url_pattern: String,
    pub response: NetMockResponse,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetObserve {
    pub tab_id: String,
    /// Optional URL regex filter, applied broker-side to observed event URLs.
    #[serde(default)]
    pub filter: Option<String>,
}

// ---------------------------------------------------------------------
// SPEC §12 U3 — browser deep-network surface.
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetInterceptFulfillWithBody {
    pub tab_id: String,
    pub pattern: String,
    pub response: NetMockResponse,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetRequestOverrides {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    /// `[[name, value], ...]`
    #[serde(default)]
    pub headers: Option<serde_json::Value>,
    #[serde(default)]
    pub post_data_base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetInterceptModifyRequest {
    pub tab_id: String,
    pub pattern: String,
    pub overrides: NetRequestOverrides,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetInterceptFail {
    pub tab_id: String,
    pub pattern: String,
    /// CDP `Network.ErrorReason` string (e.g. `NameNotResolved`,
    /// `ConnectionRefused`, `Failed`).
    pub error_reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetReplay {
    pub tab_id: String,
    /// CDP `Network.requestId` from a prior `requestWillBeSent`.
    pub request_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetWebsocketObserve {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetWebsocketInjectFrame {
    pub tab_id: String,
    /// Substring match against the WebSocket URL — selects which open
    /// connection in the page to inject through.
    pub url_substring: String,
    /// Frame payload bytes, base64-encoded.
    pub payload_base64: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetEventsourceObserve {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetHarExport {
    pub tab_id: String,
    /// Wall-clock epoch milliseconds; `0` returns every retained entry.
    #[serde(default)]
    pub since_ts: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetProxyAuth {
    pub user: String,
    pub pass: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetProxy {
    /// `"http"`, `"https"`, `"socks5"`, etc.
    pub scheme: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub auth: Option<NetProxyAuth>,
    /// Semicolon-joined hosts to bypass.
    #[serde(default)]
    pub bypass: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NetMitmCertInstall {
    /// PEM-encoded CA certificate to trust for MITM scenarios.
    pub ca_pem: String,
}

// ---------------------------------------------------------------------
// vision.* (SPEC §11 V4)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Bbox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionReadText {
    pub tab_id: String,
    /// Optional pixel rect to filter the returned regions.
    #[serde(default)]
    pub region: Option<Bbox>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionFindText {
    pub tab_id: String,
    /// Substring (default) or regex (when `is_regex = true`). Match is
    /// case-insensitive.
    pub query: String,
    #[serde(default)]
    pub is_regex: Option<bool>,
    #[serde(default)]
    pub region: Option<Bbox>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionCompare {
    pub tab_id: String,
    /// On-disk path to a reference image (PNG/JPEG). Compared against
    /// the most recent decoded frame via 32×32 average-hash distance.
    pub ref_image_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionFps {
    pub tab_id: String,
    /// Peak frames per second when an action is in flight. 1..=60.
    pub fps: u32,
    /// Optional idle FPS. Defaults to `min(5, fps)`.
    #[serde(default)]
    pub idle_fps: Option<u32>,
}

// ---------------------------------------------------------------------
// SPEC §11 V4 deeper hooks (already implemented on `VisionPipeline`).
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionStability {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionChangedSince {
    pub tab_id: String,
    /// Microseconds since epoch — only changes whose `captured_us` is
    /// strictly greater are returned.
    pub since_us: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionVerifyAction {
    pub tab_id: String,
    /// Action context: `{action, element_ref?, element_text?, note?}`.
    pub action: serde_json::Value,
}

// ---------------------------------------------------------------------
// SPEC §12 U10 sub-granularity surface.
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionPixel {
    pub tab_id: String,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionRegionClassify {
    pub tab_id: String,
    pub region: Bbox,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionColorPalette {
    pub tab_id: String,
    #[serde(default)]
    pub region: Option<Bbox>,
    /// `1..=16` dominant colours.
    pub k: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionTextStyle {
    pub tab_id: String,
    pub region: Bbox,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionLayoutSegments {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionIconRecognize {
    pub tab_id: String,
    pub region: Bbox,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionQrBarcode {
    pub tab_id: String,
    #[serde(default)]
    pub region: Option<Bbox>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionScrollbarPosition {
    pub tab_id: String,
    #[serde(default)]
    pub region: Option<Bbox>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionLoadingDetect {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionTooltipDetect {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionModalDetect {
    pub tab_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionFrameRef {
    /// `vision.frame` sequence number previously observed on this tab.
    pub seq: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionDiffSemantic {
    pub tab_id: String,
    pub prev: VisionFrameRef,
    pub next: VisionFrameRef,
    /// Action context as for `vision.verify_action`.
    pub action_context: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionAnimationFrames {
    pub tab_id: String,
    /// Window length in milliseconds. 1..=5000.
    pub duration_ms: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VisionFaceBlur {
    pub tab_id: String,
    #[serde(default)]
    pub region: Option<Bbox>,
    /// Output PNG path on disk (parent directory will be created if
    /// missing).
    pub output: String,
}

// ---------------------------------------------------------------------
// system.* (SPEC §12 U8 — host system-control surface)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemAudioOutput {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemAudioInput {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemAudioDirection {
    Input,
    Output,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemAudioSelect {
    pub direction: SystemAudioDirection,
    pub uid: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemAudioVolume {
    #[serde(default)]
    pub level: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemAudioMute {
    #[serde(default)]
    pub value: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemAudioCaptureToFile {
    pub path: String,
    pub duration_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemMicCapture {
    pub path: String,
    pub duration_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemCameraSnapshot {
    pub path: String,
    #[serde(default)]
    pub device_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemScreenCaptureRegion {
    pub path: String,
    pub x: i64,
    pub y: i64,
    pub width: u64,
    pub height: u64,
    #[serde(default)]
    pub display_id: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemScreenListDisplays {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemBluetoothScan {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemBluetoothConnect {
    pub address: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemBluetoothDisconnect {
    pub address: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemUsbDevices {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemBattery {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemNetworkInterfaces {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemNetworkRoutes {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemNetworkConnections {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemProcessList {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemProcessInfo {
    pub pid: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemProcessSignal {
    pub pid: i32,
    pub signal: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemFseventsWatch {
    pub paths: Vec<String>,
    #[serde(default)]
    pub events: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemSpotlightQuery {
    pub q: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SystemMetadata {
    pub path: String,
}

// ---------------------------------------------------------------------
// term.* (SPEC §12 U9 — terminal / PTY surface)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermSpawn {
    pub shell: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermWrite {
    pub session_id: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub data_base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermRead {
    pub session_id: String,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermSnapshot {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermResize {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermClose {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermSendSignal {
    pub session_id: String,
    pub signal: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermScrollback {
    pub session_id: String,
    pub n_lines: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermAltScreenActive {
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TermMouseEventKind {
    Press,
    Release,
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TermMouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TermMouseEvent {
    pub session_id: String,
    pub row: u16,
    pub col: u16,
    pub kind: TermMouseEventKind,
    #[serde(default)]
    pub button: Option<TermMouseButton>,
    #[serde(default)]
    pub shift: Option<bool>,
    #[serde(default)]
    pub alt: Option<bool>,
    #[serde(default)]
    pub ctrl: Option<bool>,
}

// ---------------------------------------------------------------------
// app.* (SPEC §11 V2 — universal control surface for native macOS apps)
// ---------------------------------------------------------------------

/// `app.list` — no params; returns the running native applications.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppList {}

/// `app.snapshot` — walk an app's AX tree and return a snapshot. Refs in the
/// returned `elements[]` are scoped to `(app_id, snapshot_seq)`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppSnapshot {
    /// Bundle id, e.g. `com.apple.calculator`.
    pub app_id: String,
}

/// `app.click` — `AXUIElementPerformAction(elem, kAXPressAction)`. Does not
/// activate the target app.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppClick {
    pub app_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
}

/// `app.type` — set kAXValueAttribute when settable; otherwise focus + post
/// Unicode keyboard events to the pid (no app activation).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppType {
    pub app_id: String,
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub text: String,
    /// Replace existing text instead of appending. Default false.
    #[serde(default)]
    pub clear_first: Option<bool>,
}

/// `app.scroll` — `CGEventCreateScrollWheelEvent2` posted to the pid.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppScroll {
    pub app_id: String,
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
    pub dx: f64,
    pub dy: f64,
}

/// `app.eval` — execute AppleScript via `osascript` against the bundle.
/// Bodies that contain `activate` are rejected (SPEC §5).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppEval {
    pub app_id: String,
    pub applescript: String,
}
