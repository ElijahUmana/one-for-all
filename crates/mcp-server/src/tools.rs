//! Tool dispatch table — locked to SPEC §7.
//!
//! Each tool is a (canonical name, typed input struct, description) triple.
//! `tools/list` builds its `inputSchema` from the typed struct via schemars;
//! `tools/call` validates and forwards to the broker via [`BrokerClient::call`]
//! at the canonical method name. There is **no** `tool.call` wrapper — the
//! broker exposes each tool as its own JSON-RPC method (SPEC §2).

use std::time::Duration;

use once_cell::sync::OnceCell;
use schemars::schema_for;
use serde::Serialize;
use serde_json::Value;

use crate::broker_client::{BrokerClient, CallOptions};
use crate::error::BridgeError;
use crate::schema::*;

/// Canonical method names. Order is presentation-order in `tools/list`.
pub const TOOL_NAMES: &[&str] = &[
    "browser.context.create",
    "browser.context.list",
    "browser.context.destroy",
    "tab.open",
    "tab.list",
    "tab.close",
    "tab.focus",
    "tab.navigate",
    "tab.wait",
    "page.snapshot",
    "page.screenshot",
    "page.read_text",
    "page.click",
    "page.type",
    "page.keypress",
    "page.scroll",
    "page.hover",
    "page.drag",
    "page.touch.tap",
    "page.touch.swipe",
    "page.touch.pinch",
    "page.touch.rotate",
    "page.pointer.press",
    "page.pointer.move",
    "page.pointer.release",
    "page.gesture.pinch",
    "page.gesture.rotate",
    "page.gesture.longpress",
    "page.drag.file_drop",
    "page.keyboard.shortcut",
    "page.keyboard.ime",
    "page.dead_key",
    "page.scroll.precise",
    "page.tab_traversal",
    "page.right_click_menu_navigate",
    "page.eval",
    "page.cookies",
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
    "page.viewport",
    "page.user_agent",
    "page.geo",
    "page.dark_mode",
    "page.network_conditions",
    "page.emulate",
    // SPEC §12 U4 — perf + introspection.
    "page.performance_timeline_start",
    "page.performance_timeline_stop",
    "page.performance_metrics",
    "page.coverage_js_start",
    "page.coverage_js_take",
    "page.coverage_css_start",
    "page.coverage_css_take",
    "page.heap_snapshot",
    "page.heap_sample_alloc",
    "page.cpu_profile",
    "page.layout_metrics",
    "page.paint_flash",
    // SPEC §12 U5 — print + PDF.
    "page.pdf",
    "page.print_preview",
    "net.intercept",
    "net.mock",
    "net.observe",
    // SPEC §12 U3 — browser deep-network surface.
    "net.intercept.fulfill_with_body",
    "net.intercept.modify_request",
    "net.intercept.fail",
    "net.replay",
    "net.websocket.observe",
    "net.websocket.inject_frame",
    "net.eventsource.observe",
    "net.har.export",
    "net.proxy",
    "net.mitm_cert.install",
    "vision.read_text",
    "vision.find_text",
    "vision.compare",
    "vision.fps",
    // SPEC §11 V4 deeper hooks (already implemented on `VisionPipeline`).
    "vision.stability",
    "vision.changed_since",
    "vision.verify_action",
    // SPEC §12 U10 — universal vision sub-granularity surface.
    "vision.pixel",
    "vision.region.classify",
    "vision.color.palette",
    "vision.text.style",
    "vision.layout.segments",
    "vision.icon.recognize",
    "vision.qr_barcode",
    "vision.scrollbar.position",
    "vision.loading.detect",
    "vision.tooltip.detect",
    "vision.modal.detect",
    "vision.diff.semantic",
    "vision.animation.frames",
    "vision.face_blur",
    // SPEC §12 U9 — PTY-backed terminal surface.
    "term.spawn",
    "term.write",
    "term.read",
    "term.snapshot",
    "term.resize",
    "term.close",
    "term.send_signal",
    "term.scrollback",
    "term.alt_screen_active",
    "term.mouse_event",
    // SPEC §12 U8 — host system-control surface.
    "system.audio.output",
    "system.audio.input",
    "system.audio.select",
    "system.audio.volume",
    "system.audio.mute",
    "system.audio.capture_to_file",
    "system.mic.capture",
    "system.camera.snapshot",
    "system.screen.capture_region",
    "system.screen.list_displays",
    "system.bluetooth.scan",
    "system.bluetooth.connect",
    "system.bluetooth.disconnect",
    "system.usb.devices",
    "system.battery",
    "system.network.interfaces",
    "system.network.routes",
    "system.network.connections",
    "system.process.list",
    "system.process.info",
    "system.process.signal",
    "system.fsevents.watch",
    "system.spotlight.query",
    "system.metadata",
    // SPEC §11 V2 — universal control surface for native macOS apps via the
    // system Accessibility API. Refs scoped to (app_id, snapshot_seq).
    "app.list",
    "app.snapshot",
    "app.click",
    "app.type",
    "app.scroll",
    "app.eval",
];

#[derive(Debug, Serialize, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

static TOOLS_LIST: OnceCell<Vec<ToolDescriptor>> = OnceCell::new();

pub fn list() -> &'static [ToolDescriptor] {
    TOOLS_LIST.get_or_init(build_list).as_slice()
}

fn build_list() -> Vec<ToolDescriptor> {
    macro_rules! desc {
        ($name:literal, $ty:ty, $doc:literal) => {
            ToolDescriptor {
                name: $name,
                description: $doc,
                input_schema: serde_json::to_value(schema_for!($ty)).unwrap_or(Value::Null),
            }
        };
    }
    vec![
        desc!(
            "browser.context.create",
            BrowserContextCreate,
            "Create an isolated browser context (a new Chromium with its own user-data-dir). Returns context_id."
        ),
        desc!(
            "browser.context.list",
            BrowserContextList,
            "List browser contexts owned by the current session."
        ),
        desc!(
            "browser.context.destroy",
            BrowserContextDestroy,
            "Destroy a browser context and close all its tabs."
        ),
        desc!(
            "tab.open",
            TabOpen,
            "Open a new tab and navigate to url. Optional wait_until: load|domcontentloaded|networkidle."
        ),
        desc!("tab.list", TabList, "List all tabs in the current session."),
        desc!("tab.close", TabClose, "Close a tab by id."),
        desc!(
            "tab.focus",
            TabFocus,
            "Bring a tab to the front using Page.bringToFront (no focus steal)."
        ),
        desc!(
            "tab.navigate",
            TabNavigate,
            "Navigate an existing tab to a url with an optional wait_until predicate."
        ),
        desc!(
            "tab.wait",
            TabWait,
            "Wait for a predicate: load, networkidle, {selector}, or {url_regex}."
        ),
        desc!(
            "page.snapshot",
            PageSnapshot,
            "Capture an accessibility-tree snapshot. Returned elements carry `ref` ids usable by click/type/etc."
        ),
        desc!(
            "page.screenshot",
            PageScreenshot,
            "Capture a PNG/JPEG screenshot of the viewport, full page, or a single element."
        ),
        desc!(
            "page.read_text",
            PageReadText,
            "Read visible text content for a ref or the entire body."
        ),
        desc!(
            "page.click",
            PageClick,
            "Click an element by ref. Use realistic=true for a humanized Bezier mouse path."
        ),
        desc!(
            "page.type",
            PageType,
            "Type text into an element ref with optional per-character delay."
        ),
        desc!(
            "page.keypress",
            PageKeypress,
            "Send a single key press with optional modifiers (Alt/Control/Meta/Shift)."
        ),
        desc!(
            "page.scroll",
            PageScroll,
            "Scroll the page or a ref'd element by (dx, dy) pixels."
        ),
        desc!(
            "page.hover",
            PageHover,
            "Hover over a ref'd element."
        ),
        desc!(
            "page.drag",
            PageDrag,
            "Mouse-drag from one ref to another."
        ),
        desc!(
            "page.touch.tap",
            PageTouchTap,
            "SPEC §12 U1 — synthesize a tap gesture at CSS-pixel coordinates."
        ),
        desc!(
            "page.touch.swipe",
            PageTouchSwipe,
            "SPEC §12 U1 — synthesize a one-finger swipe between CSS-pixel coordinates."
        ),
        desc!(
            "page.touch.pinch",
            PageTouchPinch,
            "SPEC §12 U1 — synthesize a two-finger pinch centered on a CSS-pixel point."
        ),
        desc!(
            "page.touch.rotate",
            PageTouchRotate,
            "SPEC §12 U1 — synthesize a two-finger rotate gesture around a CSS-pixel point."
        ),
        desc!(
            "page.pointer.press",
            PagePointerPress,
            "SPEC §12 U1 — press a pen/pointer contact with optional pressure and tilt."
        ),
        desc!(
            "page.pointer.move",
            PagePointerMove,
            "SPEC §12 U1 — move a pen/pointer contact with optional pressure and tilt."
        ),
        desc!(
            "page.pointer.release",
            PagePointerRelease,
            "SPEC §12 U1 — release a pen/pointer contact with optional pressure and tilt."
        ),
        desc!(
            "page.gesture.pinch",
            PageGesturePinch,
            "SPEC §12 U1 — compose a higher-level pinch gesture from touch events."
        ),
        desc!(
            "page.gesture.rotate",
            PageGestureRotate,
            "SPEC §12 U1 — compose a higher-level rotation gesture from touch events."
        ),
        desc!(
            "page.gesture.longpress",
            PageGestureLongpress,
            "SPEC §12 U1 — compose a long-press gesture at CSS-pixel coordinates."
        ),
        desc!(
            "page.drag.file_drop",
            PageDragFileDrop,
            "SPEC §12 U1 — dispatch a real file drop into a page target ref."
        ),
        desc!(
            "page.keyboard.shortcut",
            PageKeyboardShortcut,
            "SPEC §12 U1 — dispatch a keyboard accelerator like cmd+s with modifiers."
        ),
        desc!(
            "page.keyboard.ime",
            PageKeyboardIme,
            "SPEC §12 U1 — drive IME composition state and commit text."
        ),
        desc!(
            "page.dead_key",
            PageDeadKey,
            "SPEC §12 U1 — compose an accent dead key with a base character."
        ),
        desc!(
            "page.scroll.precise",
            PageScrollPrecise,
            "SPEC §12 U1 — dispatch stepped wheel scrolling with optional momentum and easing."
        ),
        desc!(
            "page.tab_traversal",
            PageTabTraversal,
            "SPEC §12 U1 — traverse focus forward/backward using Tab or Shift+Tab."
        ),
        desc!(
            "page.right_click_menu_navigate",
            PageRightClickMenuNavigate,
            "SPEC §12 U1 — right-click a ref and activate a context-menu path."
        ),
        desc!(
            "page.eval",
            PageEval,
            "Evaluate a JS expression in the tab. Requires the session 'eval' capability; optional ref validates target freshness before evaluation."
        ),
        desc!(
            "page.cookies",
            PageCookies,
            "Get, set, or clear cookies in the tab's context."
        ),
        desc!(
            "page.cookies.deep_set",
            PageCookiesDeepSet,
            "SPEC §12 U2 — set one cookie with full CDP attributes including partition_key."
        ),
        desc!(
            "page.storage",
            PageStorage,
            "Operate on local/session storage or IndexedDB via the legacy umbrella shape."
        ),
        desc!(
            "page.localstorage.get",
            PageStorageGet,
            "SPEC §12 U2 — get one localStorage key or dump the full localStorage map."
        ),
        desc!(
            "page.localstorage.set",
            PageStorageSet,
            "SPEC §12 U2 — set one localStorage key to a string value."
        ),
        desc!(
            "page.localstorage.delete",
            PageStorageDelete,
            "SPEC §12 U2 — delete one localStorage key."
        ),
        desc!(
            "page.localstorage.clear",
            PageStorageClear,
            "SPEC §12 U2 — clear localStorage for the active document origin."
        ),
        desc!(
            "page.localstorage.cas",
            PageStorageCas,
            "SPEC §12 U2 — compare-and-set one localStorage key."
        ),
        desc!(
            "page.sessionstorage.get",
            PageStorageGet,
            "SPEC §12 U2 — get one sessionStorage key or dump the full sessionStorage map."
        ),
        desc!(
            "page.sessionstorage.set",
            PageStorageSet,
            "SPEC §12 U2 — set one sessionStorage key to a string value."
        ),
        desc!(
            "page.sessionstorage.delete",
            PageStorageDelete,
            "SPEC §12 U2 — delete one sessionStorage key."
        ),
        desc!(
            "page.sessionstorage.clear",
            PageStorageClear,
            "SPEC §12 U2 — clear sessionStorage for the active document."
        ),
        desc!(
            "page.sessionstorage.cas",
            PageStorageCas,
            "SPEC §12 U2 — compare-and-set one sessionStorage key."
        ),
        desc!(
            "page.indexeddb.list_databases",
            PageIndexeddbListDatabases,
            "SPEC §12 U2 — list IndexedDB database names for the active document origin."
        ),
        desc!(
            "page.indexeddb.list_stores",
            PageIndexeddbListStores,
            "SPEC §12 U2 — list object stores for one IndexedDB database."
        ),
        desc!(
            "page.indexeddb.query",
            PageIndexeddbQuery,
            "SPEC §12 U2 — query IndexedDB object store or index data."
        ),
        desc!(
            "page.indexeddb.put",
            PageIndexeddbPut,
            "SPEC §12 U2 — put one IndexedDB record via a same-origin bootstrap page."
        ),
        desc!(
            "page.indexeddb.delete",
            PageIndexeddbDelete,
            "SPEC §12 U2 — delete IndexedDB records in a key range."
        ),
        desc!(
            "page.indexeddb.delete_database",
            PageIndexeddbDeleteDatabase,
            "SPEC §12 U2 — delete an IndexedDB database."
        ),
        desc!(
            "page.cache_api.list",
            PageCacheApiList,
            "SPEC §12 U2 — list CacheStorage caches for the active document origin."
        ),
        desc!(
            "page.cache_api.inspect",
            PageCacheApiInspect,
            "SPEC §12 U2 — inspect one cache entry or page through cache contents."
        ),
        desc!(
            "page.cache_api.delete",
            PageCacheApiDelete,
            "SPEC §12 U2 — delete a whole cache or one cache entry."
        ),
        desc!(
            "page.permissions.query",
            PagePermissionsQuery,
            "SPEC §12 U2 — query one browser permission via the Permissions API."
        ),
        desc!(
            "page.permissions.grant",
            PagePermissionsGrant,
            "SPEC §12 U2 — grant one browser permission override. Requires session capability \"storage_state\"."
        ),
        desc!(
            "page.permissions.revoke",
            PagePermissionsRevoke,
            "SPEC §12 U2 — revoke one permission override or reset all overrides. Requires session capability \"storage_state\"."
        ),
        desc!(
            "page.storage.quota",
            PageStorageQuota,
            "SPEC §12 U2 — return storage usage and quota for the active document origin."
        ),
        desc!(
            "page.viewport",
            PageViewport,
            "Override viewport size, device-scale-factor, and mobile flag."
        ),
        desc!(
            "page.user_agent",
            PageUserAgent,
            "Override user-agent and optional client hints."
        ),
        desc!(
            "page.geo",
            PageGeo,
            "Override the geolocation reported to the page."
        ),
        desc!(
            "page.dark_mode",
            PageDarkMode,
            "Override prefers-color-scheme."
        ),
        desc!(
            "page.network_conditions",
            PageNetworkConditions,
            "Emulate network conditions (offline, latency, throughput) per SPEC §10 M7."
        ),
        desc!(
            "page.emulate",
            PageEmulate,
            "Emulate locale, timezone, and CPU throttle per SPEC §10 M8."
        ),
        // ----- SPEC §12 U4 — perf + introspection -----
        desc!(
            "page.performance_timeline_start",
            PagePerformanceTimelineStart,
            "SPEC §12 U4 — start a Tracing.start session in stream mode. Returns when Chromium accepts the categories."
        ),
        desc!(
            "page.performance_timeline_stop",
            PagePerformanceTimelineStop,
            "SPEC §12 U4 — stop tracing, drain Tracing.tracingComplete via IO.read, return {trace_path, bytes, data_loss}."
        ),
        desc!(
            "page.performance_metrics",
            PagePerformanceMetrics,
            "SPEC §12 U4 — Performance.getMetrics. Returns {metrics: [{name, value}, …]}."
        ),
        desc!(
            "page.coverage_js_start",
            PageCoverageJsStart,
            "SPEC §12 U4 — Profiler.startPreciseCoverage with optional call_count + detailed flags."
        ),
        desc!(
            "page.coverage_js_take",
            PageCoverageJsTake,
            "SPEC §12 U4 — Profiler.takePreciseCoverage. Returns {result, timestamp}."
        ),
        desc!(
            "page.coverage_css_start",
            PageCoverageCssStart,
            "SPEC §12 U4 — CSS.startRuleUsageTracking."
        ),
        desc!(
            "page.coverage_css_take",
            PageCoverageCssTake,
            "SPEC §12 U4 — CSS.takeCoverageDelta. Returns {coverage, timestamp}."
        ),
        desc!(
            "page.heap_snapshot",
            PageHeapSnapshot,
            "SPEC §12 U4 — HeapProfiler.takeHeapSnapshot. Streams chunks to disk; returns {snapshot_path, bytes}."
        ),
        desc!(
            "page.heap_sample_alloc",
            PageHeapSampleAlloc,
            "SPEC §12 U4 — HeapProfiler.startSampling, sleep duration_ms, stopSampling. Returns {profile}."
        ),
        desc!(
            "page.cpu_profile",
            PageCpuProfile,
            "SPEC §12 U4 — Profiler.start, sleep duration_ms, Profiler.stop. Returns {profile}."
        ),
        desc!(
            "page.layout_metrics",
            PageLayoutMetrics,
            "SPEC §12 U4 — Page.getLayoutMetrics. Returns viewport + content_size in CSS px."
        ),
        desc!(
            "page.paint_flash",
            PagePaintFlash,
            "SPEC §12 U4 — Overlay.setShowPaintRects. Highlights repainted regions on the live tab."
        ),
        // ----- SPEC §12 U5 — print + PDF -----
        desc!(
            "page.pdf",
            PagePdf,
            "SPEC §12 U5 — Page.printToPDF. Auto-streams to disk for large docs; small docs returned inline as base64."
        ),
        desc!(
            "page.print_preview",
            PagePrintPreview,
            "SPEC §12 U5 — capture a screenshot under emulated @media print. Restores media state on exit."
        ),
        desc!(
            "net.intercept",
            NetIntercept,
            "Set a network interception action: continue, fulfill, or fail."
        ),
        desc!(
            "net.mock",
            NetMock,
            "Serve a mocked response for matching requests until removed."
        ),
        desc!(
            "net.observe",
            NetObserve,
            "Subscribe to network.request and network.response notifications, optionally filtered by a URL regex."
        ),
        // SPEC §12 U3 — browser deep-network surface.
        desc!(
            "net.intercept.fulfill_with_body",
            NetInterceptFulfillWithBody,
            "SPEC §12 U3 — serve a custom HTTP response for matching requests via Fetch.fulfillRequest."
        ),
        desc!(
            "net.intercept.modify_request",
            NetInterceptModifyRequest,
            "SPEC §12 U3 — rewrite URL/method/headers/body for matching requests via Fetch.continueRequest with overrides."
        ),
        desc!(
            "net.intercept.fail",
            NetInterceptFail,
            "SPEC §12 U3 — fail matching requests with the given CDP errorReason via Fetch.failRequest."
        ),
        desc!(
            "net.replay",
            NetReplay,
            "SPEC §12 U3 — replay a prior XHR via Network.replayXHR."
        ),
        desc!(
            "net.websocket.observe",
            NetWebsocketObserve,
            "SPEC §12 U3 — observe WebSocket lifecycle + frames (sent/received/error/close)."
        ),
        desc!(
            "net.websocket.inject_frame",
            NetWebsocketInjectFrame,
            "SPEC §12 U3 — inject a WebSocket frame via the Runtime.evaluate fallback. The page now self-arms the WebSocket registry shim before injection."
        ),
        desc!(
            "net.eventsource.observe",
            NetEventsourceObserve,
            "SPEC §12 U3 — observe Server-Sent-Events messages via Network.eventSourceMessageReceived."
        ),
        desc!(
            "net.har.export",
            NetHarExport,
            "SPEC §12 U3 — export captured network activity as a HAR 1.2 log."
        ),
        desc!(
            "net.proxy",
            NetProxy,
            "SPEC §12 U3 — stage a proxy config; applied at next Browser::launch via --proxy-server."
        ),
        desc!(
            "net.mitm_cert.install",
            NetMitmCertInstall,
            "SPEC §12 U3 — install a CA cert into the per-session trust store. macOS Chrome uses the native KeyChain — see error for the manual `security` command."
        ),
        desc!(
            "vision.read_text",
            VisionReadText,
            "SPEC §11 V4 — return cached OCR text regions for a tab. Optional region filter."
        ),
        desc!(
            "vision.find_text",
            VisionFindText,
            "SPEC §11 V4 — substring or regex search across cached OCR regions; sub-10ms p99."
        ),
        desc!(
            "vision.compare",
            VisionCompare,
            "SPEC §11 V4 — perceptual-hash similarity between the latest captured frame and a reference image on disk."
        ),
        desc!(
            "vision.fps",
            VisionFps,
            "SPEC §11 V4 — change the active/idle FPS pair for a tab's continuous capture loop."
        ),
        desc!(
            "vision.stability",
            VisionStability,
            "SPEC §11 V4 — return the rolling layout-stability score (loading/settling/stable) and seq."
        ),
        desc!(
            "vision.changed_since",
            VisionChangedSince,
            "SPEC §11 V4 — union of changed tiles whose captured_us is greater than `since_us`."
        ),
        desc!(
            "vision.verify_action",
            VisionVerifyAction,
            "SPEC §11 V4 — VLM verdict for an in-flight action (no-op when VLM is off)."
        ),
        desc!(
            "vision.pixel",
            VisionPixel,
            "SPEC §12 U10 — direct mmap RGBA8 read at (x, y) in CSS px from the latest frame."
        ),
        desc!(
            "vision.region.classify",
            VisionRegionClassify,
            "SPEC §12 U10 — heuristic region typing: text / image / icon / video / control."
        ),
        desc!(
            "vision.color.palette",
            VisionColorPalette,
            "SPEC §12 U10 — k-means quantization of a region into top-k dominant colours."
        ),
        desc!(
            "vision.text.style",
            VisionTextStyle,
            "SPEC §12 U10 — infer font size/weight/colour for the OCR region under a rect."
        ),
        desc!(
            "vision.layout.segments",
            VisionLayoutSegments,
            "SPEC §12 U10 — page → rows → cards segmentation tree from edge projections."
        ),
        desc!(
            "vision.icon.recognize",
            VisionIconRecognize,
            "SPEC §12 U10 — match a small region against the local icon library."
        ),
        desc!(
            "vision.qr_barcode",
            VisionQrBarcode,
            "SPEC §12 U10 — Apple Vision barcode/QR detection (empty off-Apple)."
        ),
        desc!(
            "vision.scrollbar.position",
            VisionScrollbarPosition,
            "SPEC §12 U10 — locate the vertical scrollbar thumb and its 0..=1 position."
        ),
        desc!(
            "vision.loading.detect",
            VisionLoadingDetect,
            "SPEC §12 U10 — classify motion-over-window as idle / progress / spinner."
        ),
        desc!(
            "vision.tooltip.detect",
            VisionTooltipDetect,
            "SPEC §12 U10 — small-overlay heuristic over recent change history."
        ),
        desc!(
            "vision.modal.detect",
            VisionModalDetect,
            "SPEC §12 U10 — dim-overlay + bright-body heuristic for modal dialogs."
        ),
        desc!(
            "vision.diff.semantic",
            VisionDiffSemantic,
            "SPEC §12 U10 — VLM-driven semantic diff between two prior vision.frame seqs: no_op / progress / failure / success."
        ),
        desc!(
            "vision.animation.frames",
            VisionAnimationFrames,
            "SPEC §12 U10 — return frame handles for the last `duration_ms` window."
        ),
        desc!(
            "vision.face_blur",
            VisionFaceBlur,
            "SPEC §12 U10 — detect & Gaussian-blur faces; gated by capability `face_detect`."
        ),
        desc!(
            "term.spawn",
            TermSpawn,
            "SPEC §12 U9 — spawn a real PTY session running the requested shell inside the broker session sandbox."
        ),
        desc!(
            "term.write",
            TermWrite,
            "SPEC §12 U9 — write UTF-8 text or raw base64-decoded bytes into a PTY session."
        ),
        desc!(
            "term.read",
            TermRead,
            "SPEC §12 U9 — read buffered PTY output; returns base64 plus UTF-8 text when decodable."
        ),
        desc!(
            "term.snapshot",
            TermSnapshot,
            "SPEC §12 U9 — return the parser-maintained terminal screen, cursor, attrs, alt-screen, and exit state."
        ),
        desc!(
            "term.resize",
            TermResize,
            "SPEC §12 U9 — resize the PTY and the maintained screen model to the requested cols/rows."
        ),
        desc!(
            "term.close",
            TermClose,
            "SPEC §12 U9 — close a PTY session and return its final exit state."
        ),
        desc!(
            "term.send_signal",
            TermSendSignal,
            "SPEC §12 U9 — send a signal to the PTY foreground process group (or child pid fallback)."
        ),
        desc!(
            "term.scrollback",
            TermScrollback,
            "SPEC §12 U9 — return bounded terminal scrollback lines from the maintained primary-screen history."
        ),
        desc!(
            "term.alt_screen_active",
            TermAltScreenActive,
            "SPEC §12 U9 — report whether the terminal is in alt-screen mode plus active mouse-tracking state."
        ),
        desc!(
            "term.mouse_event",
            TermMouseEvent,
            "SPEC §12 U9 — inject an xterm mouse event into the PTY when mouse tracking is enabled."
        ),
        desc!(
            "system.audio.output",
            SystemAudioOutput,
            "SPEC §12 U8 — list host audio output devices. Requires session capability \"system\"."
        ),
        desc!(
            "system.audio.input",
            SystemAudioInput,
            "SPEC §12 U8 — list host audio input devices. Requires capabilities \"system\" and \"mic\"."
        ),
        desc!(
            "system.audio.select",
            SystemAudioSelect,
            "SPEC §12 U8 — resolve/select a host audio device by uid for input or output."
        ),
        desc!(
            "system.audio.volume",
            SystemAudioVolume,
            "SPEC §12 U8 — get or set the host output volume (0-100)."
        ),
        desc!(
            "system.audio.mute",
            SystemAudioMute,
            "SPEC §12 U8 — get or set host output mute state."
        ),
        desc!(
            "system.audio.capture_to_file",
            SystemAudioCaptureToFile,
            "SPEC §12 U8 — capture host/system audio to a session-relative file path. Requires capabilities \"system\" and \"screen\"."
        ),
        desc!(
            "system.mic.capture",
            SystemMicCapture,
            "SPEC §12 U8 — capture microphone audio to a session-relative file path. Requires capabilities \"system\" and \"mic\"."
        ),
        desc!(
            "system.camera.snapshot",
            SystemCameraSnapshot,
            "SPEC §12 U8 — capture a JPEG camera snapshot to a session-relative path. Returns image content inline. Requires capabilities \"system\" and \"camera\"."
        ),
        desc!(
            "system.screen.capture_region",
            SystemScreenCaptureRegion,
            "SPEC §12 U8 — capture a screen region in global host coordinates to a session-relative PNG path. Returns image content inline. Requires capabilities \"system\" and \"screen\"."
        ),
        desc!(
            "system.screen.list_displays",
            SystemScreenListDisplays,
            "SPEC §12 U8 — list active host displays with ids, origins, dimensions, and scale factors."
        ),
        desc!(
            "system.bluetooth.scan",
            SystemBluetoothScan,
            "SPEC §12 U8 — scan for nearby Bluetooth devices. Requires capabilities \"system\" and \"bluetooth\"."
        ),
        desc!(
            "system.bluetooth.connect",
            SystemBluetoothConnect,
            "SPEC §12 U8 — connect to a Bluetooth device by address. Requires capabilities \"system\" and \"bluetooth\"."
        ),
        desc!(
            "system.bluetooth.disconnect",
            SystemBluetoothDisconnect,
            "SPEC §12 U8 — disconnect from a Bluetooth device by address. Requires capabilities \"system\" and \"bluetooth\"."
        ),
        desc!(
            "system.usb.devices",
            SystemUsbDevices,
            "SPEC §12 U8 — enumerate USB devices. Requires capabilities \"system\" and \"raw_usb\"."
        ),
        desc!(
            "system.battery",
            SystemBattery,
            "SPEC §12 U8 — report host battery state and power source information."
        ),
        desc!(
            "system.network.interfaces",
            SystemNetworkInterfaces,
            "SPEC §12 U8 — list host network interfaces."
        ),
        desc!(
            "system.network.routes",
            SystemNetworkRoutes,
            "SPEC §12 U8 — list host routing table entries."
        ),
        desc!(
            "system.network.connections",
            SystemNetworkConnections,
            "SPEC §12 U8 — list live TCP network connections from lsof."
        ),
        desc!(
            "system.process.list",
            SystemProcessList,
            "SPEC §12 U8 — list host processes with pid/ppid/uid/name summary fields."
        ),
        desc!(
            "system.process.info",
            SystemProcessInfo,
            "SPEC §12 U8 — get detailed information for one host process pid."
        ),
        desc!(
            "system.process.signal",
            SystemProcessSignal,
            "SPEC §12 U8 — send a Unix signal to one host process pid."
        ),
        desc!(
            "system.fsevents.watch",
            SystemFseventsWatch,
            "SPEC §12 U8 — start a session-owned filesystem watch and stream `event/notify {topic:\"system.fsevents\"}` notifications."
        ),
        desc!(
            "system.spotlight.query",
            SystemSpotlightQuery,
            "SPEC §12 U8 — query Spotlight via mdfind."
        ),
        desc!(
            "system.metadata",
            SystemMetadata,
            "SPEC §12 U8 — fetch file metadata via mdls."
        ),
        desc!(
            "app.list",
            AppList,
            "SPEC §11 V2 — list running native macOS apps (bundle_id, pid, name, has_focus). Requires session capability \"native\" + AX permission."
        ),
        desc!(
            "app.snapshot",
            AppSnapshot,
            "SPEC §11 V2 — capture a native app's accessibility tree. Returned elements carry `ref` ids usable by app.click/type/scroll."
        ),
        desc!(
            "app.click",
            AppClick,
            "SPEC §11 V2 — click an element in a native app via AXPress. Does not raise the target app to the foreground."
        ),
        desc!(
            "app.type",
            AppType,
            "SPEC §11 V2 — type text into a native-app element via AX value-set or pid-targeted Unicode keystrokes (no app activation)."
        ),
        desc!(
            "app.scroll",
            AppScroll,
            "SPEC §11 V2 — scroll a native app by (dx, dy) pixels via pid-targeted CGScrollWheel events."
        ),
        desc!(
            "app.eval",
            AppEval,
            "SPEC §11 V2 — execute AppleScript against the bundle via osascript. Bodies that activate the target app are rejected."
        ),
    ]
}

pub fn find(name: &str) -> Option<&'static ToolDescriptor> {
    list().iter().find(|t| t.name == name)
}

fn call_options_for(tool: &str) -> CallOptions {
    let dur = match tool {
        "tab.wait" => Duration::from_secs(60),
        "tab.open" | "tab.navigate" | "term.spawn" => Duration::from_secs(45),
        "page.snapshot" | "page.screenshot" | "term.read" | "term.snapshot" => {
            Duration::from_secs(30)
        }
        "system.audio.capture_to_file" | "system.mic.capture" | "system.bluetooth.scan" => {
            Duration::from_secs(60)
        }
        // SPEC §12 U4 + U5 — long-tail perf operations.
        "page.pdf" | "page.heap_snapshot" | "page.performance_timeline_stop" => {
            Duration::from_secs(120)
        }
        "page.cpu_profile" | "page.heap_sample_alloc" => Duration::from_secs(180),
        _ => Duration::from_secs(30),
    };
    CallOptions { timeout: dur }
}

/// Validate `args` matches the canonical struct, then forward to the broker
/// using the tool name as the JSON-RPC method (SPEC §2).
///
// CANCELLATION: inherits from BrokerClient::call — conditional. See
// broker_client.rs for the at-most-once caveat.
pub async fn dispatch(
    broker: &BrokerClient,
    name: &str,
    args: Value,
) -> Result<Value, BridgeError> {
    let descriptor = find(name).ok_or(BridgeError::Protocol(format!("unknown tool: {name}")))?;
    validate_args(name, &args)?;
    let opts = call_options_for(name);
    // The broker uses the tool name *as* the JSON-RPC method.
    broker.call(descriptor.name, args, opts).await
}

fn validate_args(name: &str, args: &Value) -> Result<(), BridgeError> {
    fn check<T: for<'de> serde::Deserialize<'de>>(
        method: &'static str,
        v: &Value,
    ) -> Result<(), BridgeError> {
        serde_json::from_value::<T>(v.clone())
            .map(|_| ())
            .map_err(|e| BridgeError::InvalidParams {
                method,
                reason: e.to_string(),
            })
    }
    match name {
        "browser.context.create" => check::<BrowserContextCreate>(name_static(name), args),
        "browser.context.list" => check::<BrowserContextList>(name_static(name), args),
        "browser.context.destroy" => check::<BrowserContextDestroy>(name_static(name), args),
        "tab.open" => check::<TabOpen>(name_static(name), args),
        "tab.list" => check::<TabList>(name_static(name), args),
        "tab.close" => check::<TabClose>(name_static(name), args),
        "tab.focus" => check::<TabFocus>(name_static(name), args),
        "tab.navigate" => check::<TabNavigate>(name_static(name), args),
        "tab.wait" => check::<TabWait>(name_static(name), args),
        "page.snapshot" => check::<PageSnapshot>(name_static(name), args),
        "page.screenshot" => check::<PageScreenshot>(name_static(name), args),
        "page.read_text" => check::<PageReadText>(name_static(name), args),
        "page.click" => check::<PageClick>(name_static(name), args),
        "page.type" => check::<PageType>(name_static(name), args),
        "page.keypress" => check::<PageKeypress>(name_static(name), args),
        "page.scroll" => check::<PageScroll>(name_static(name), args),
        "page.hover" => check::<PageHover>(name_static(name), args),
        "page.drag" => check::<PageDrag>(name_static(name), args),
        "page.touch.tap" => check::<PageTouchTap>(name_static(name), args),
        "page.touch.swipe" => check::<PageTouchSwipe>(name_static(name), args),
        "page.touch.pinch" => check::<PageTouchPinch>(name_static(name), args),
        "page.touch.rotate" => check::<PageTouchRotate>(name_static(name), args),
        "page.pointer.press" => check::<PagePointerPress>(name_static(name), args),
        "page.pointer.move" => check::<PagePointerMove>(name_static(name), args),
        "page.pointer.release" => check::<PagePointerRelease>(name_static(name), args),
        "page.gesture.pinch" => check::<PageGesturePinch>(name_static(name), args),
        "page.gesture.rotate" => check::<PageGestureRotate>(name_static(name), args),
        "page.gesture.longpress" => check::<PageGestureLongpress>(name_static(name), args),
        "page.drag.file_drop" => check::<PageDragFileDrop>(name_static(name), args),
        "page.keyboard.shortcut" => check::<PageKeyboardShortcut>(name_static(name), args),
        "page.keyboard.ime" => check::<PageKeyboardIme>(name_static(name), args),
        "page.dead_key" => check::<PageDeadKey>(name_static(name), args),
        "page.scroll.precise" => check::<PageScrollPrecise>(name_static(name), args),
        "page.tab_traversal" => check::<PageTabTraversal>(name_static(name), args),
        "page.right_click_menu_navigate" => {
            check::<PageRightClickMenuNavigate>(name_static(name), args)
        }
        "page.eval" => check::<PageEval>(name_static(name), args),
        "page.cookies" => check::<PageCookies>(name_static(name), args),
        "page.cookies.deep_set" => check::<PageCookiesDeepSet>(name_static(name), args),
        "page.storage" => check::<PageStorage>(name_static(name), args),
        "page.localstorage.get" => check::<PageStorageGet>(name_static(name), args),
        "page.localstorage.set" => check::<PageStorageSet>(name_static(name), args),
        "page.localstorage.delete" => check::<PageStorageDelete>(name_static(name), args),
        "page.localstorage.clear" => check::<PageStorageClear>(name_static(name), args),
        "page.localstorage.cas" => check::<PageStorageCas>(name_static(name), args),
        "page.sessionstorage.get" => check::<PageStorageGet>(name_static(name), args),
        "page.sessionstorage.set" => check::<PageStorageSet>(name_static(name), args),
        "page.sessionstorage.delete" => check::<PageStorageDelete>(name_static(name), args),
        "page.sessionstorage.clear" => check::<PageStorageClear>(name_static(name), args),
        "page.sessionstorage.cas" => check::<PageStorageCas>(name_static(name), args),
        "page.indexeddb.list_databases" => {
            check::<PageIndexeddbListDatabases>(name_static(name), args)
        }
        "page.indexeddb.list_stores" => check::<PageIndexeddbListStores>(name_static(name), args),
        "page.indexeddb.query" => check::<PageIndexeddbQuery>(name_static(name), args),
        "page.indexeddb.put" => check::<PageIndexeddbPut>(name_static(name), args),
        "page.indexeddb.delete" => check::<PageIndexeddbDelete>(name_static(name), args),
        "page.indexeddb.delete_database" => {
            check::<PageIndexeddbDeleteDatabase>(name_static(name), args)
        }
        "page.cache_api.list" => check::<PageCacheApiList>(name_static(name), args),
        "page.cache_api.inspect" => check::<PageCacheApiInspect>(name_static(name), args),
        "page.cache_api.delete" => check::<PageCacheApiDelete>(name_static(name), args),
        "page.permissions.query" => check::<PagePermissionsQuery>(name_static(name), args),
        "page.permissions.grant" => check::<PagePermissionsGrant>(name_static(name), args),
        "page.permissions.revoke" => check::<PagePermissionsRevoke>(name_static(name), args),
        "page.storage.quota" => check::<PageStorageQuota>(name_static(name), args),
        "page.viewport" => check::<PageViewport>(name_static(name), args),
        "page.user_agent" => check::<PageUserAgent>(name_static(name), args),
        "page.geo" => check::<PageGeo>(name_static(name), args),
        "page.dark_mode" => check::<PageDarkMode>(name_static(name), args),
        "page.network_conditions" => check::<PageNetworkConditions>(name_static(name), args),
        "page.emulate" => check::<PageEmulate>(name_static(name), args),
        // SPEC §12 U4 — perf + introspection.
        "page.performance_timeline_start" => {
            check::<PagePerformanceTimelineStart>(name_static(name), args)
        }
        "page.performance_timeline_stop" => {
            check::<PagePerformanceTimelineStop>(name_static(name), args)
        }
        "page.performance_metrics" => check::<PagePerformanceMetrics>(name_static(name), args),
        "page.coverage_js_start" => check::<PageCoverageJsStart>(name_static(name), args),
        "page.coverage_js_take" => check::<PageCoverageJsTake>(name_static(name), args),
        "page.coverage_css_start" => check::<PageCoverageCssStart>(name_static(name), args),
        "page.coverage_css_take" => check::<PageCoverageCssTake>(name_static(name), args),
        "page.heap_snapshot" => check::<PageHeapSnapshot>(name_static(name), args),
        "page.heap_sample_alloc" => check::<PageHeapSampleAlloc>(name_static(name), args),
        "page.cpu_profile" => check::<PageCpuProfile>(name_static(name), args),
        "page.layout_metrics" => check::<PageLayoutMetrics>(name_static(name), args),
        "page.paint_flash" => check::<PagePaintFlash>(name_static(name), args),
        // SPEC §12 U5 — print + PDF.
        "page.pdf" => check::<PagePdf>(name_static(name), args),
        "page.print_preview" => check::<PagePrintPreview>(name_static(name), args),
        "net.intercept" => check::<NetIntercept>(name_static(name), args),
        "net.mock" => check::<NetMock>(name_static(name), args),
        "net.observe" => check::<NetObserve>(name_static(name), args),
        // SPEC §12 U3 — browser deep-network surface.
        "net.intercept.fulfill_with_body" => {
            check::<NetInterceptFulfillWithBody>(name_static(name), args)
        }
        "net.intercept.modify_request" => {
            check::<NetInterceptModifyRequest>(name_static(name), args)
        }
        "net.intercept.fail" => check::<NetInterceptFail>(name_static(name), args),
        "net.replay" => check::<NetReplay>(name_static(name), args),
        "net.websocket.observe" => check::<NetWebsocketObserve>(name_static(name), args),
        "net.websocket.inject_frame" => check::<NetWebsocketInjectFrame>(name_static(name), args),
        "net.eventsource.observe" => check::<NetEventsourceObserve>(name_static(name), args),
        "net.har.export" => check::<NetHarExport>(name_static(name), args),
        "net.proxy" => check::<NetProxy>(name_static(name), args),
        "net.mitm_cert.install" => check::<NetMitmCertInstall>(name_static(name), args),
        "vision.read_text" => check::<VisionReadText>(name_static(name), args),
        "vision.find_text" => check::<VisionFindText>(name_static(name), args),
        "vision.compare" => check::<VisionCompare>(name_static(name), args),
        "vision.fps" => check::<VisionFps>(name_static(name), args),
        "vision.stability" => check::<VisionStability>(name_static(name), args),
        "vision.changed_since" => check::<VisionChangedSince>(name_static(name), args),
        "vision.verify_action" => check::<VisionVerifyAction>(name_static(name), args),
        "vision.pixel" => check::<VisionPixel>(name_static(name), args),
        "vision.region.classify" => check::<VisionRegionClassify>(name_static(name), args),
        "vision.color.palette" => check::<VisionColorPalette>(name_static(name), args),
        "vision.text.style" => check::<VisionTextStyle>(name_static(name), args),
        "vision.layout.segments" => check::<VisionLayoutSegments>(name_static(name), args),
        "vision.icon.recognize" => check::<VisionIconRecognize>(name_static(name), args),
        "vision.qr_barcode" => check::<VisionQrBarcode>(name_static(name), args),
        "vision.scrollbar.position" => check::<VisionScrollbarPosition>(name_static(name), args),
        "vision.loading.detect" => check::<VisionLoadingDetect>(name_static(name), args),
        "vision.tooltip.detect" => check::<VisionTooltipDetect>(name_static(name), args),
        "vision.modal.detect" => check::<VisionModalDetect>(name_static(name), args),
        "vision.diff.semantic" => check::<VisionDiffSemantic>(name_static(name), args),
        "vision.animation.frames" => check::<VisionAnimationFrames>(name_static(name), args),
        "vision.face_blur" => check::<VisionFaceBlur>(name_static(name), args),
        "term.spawn" => check::<TermSpawn>(name_static(name), args),
        "term.write" => check::<TermWrite>(name_static(name), args),
        "term.read" => check::<TermRead>(name_static(name), args),
        "term.snapshot" => check::<TermSnapshot>(name_static(name), args),
        "term.resize" => check::<TermResize>(name_static(name), args),
        "term.close" => check::<TermClose>(name_static(name), args),
        "term.send_signal" => check::<TermSendSignal>(name_static(name), args),
        "term.scrollback" => check::<TermScrollback>(name_static(name), args),
        "term.alt_screen_active" => check::<TermAltScreenActive>(name_static(name), args),
        "term.mouse_event" => check::<TermMouseEvent>(name_static(name), args),
        "system.audio.output" => check::<SystemAudioOutput>(name_static(name), args),
        "system.audio.input" => check::<SystemAudioInput>(name_static(name), args),
        "system.audio.select" => check::<SystemAudioSelect>(name_static(name), args),
        "system.audio.volume" => check::<SystemAudioVolume>(name_static(name), args),
        "system.audio.mute" => check::<SystemAudioMute>(name_static(name), args),
        "system.audio.capture_to_file" => {
            check::<SystemAudioCaptureToFile>(name_static(name), args)
        }
        "system.mic.capture" => check::<SystemMicCapture>(name_static(name), args),
        "system.camera.snapshot" => check::<SystemCameraSnapshot>(name_static(name), args),
        "system.screen.capture_region" => {
            check::<SystemScreenCaptureRegion>(name_static(name), args)
        }
        "system.screen.list_displays" => check::<SystemScreenListDisplays>(name_static(name), args),
        "system.bluetooth.scan" => check::<SystemBluetoothScan>(name_static(name), args),
        "system.bluetooth.connect" => check::<SystemBluetoothConnect>(name_static(name), args),
        "system.bluetooth.disconnect" => {
            check::<SystemBluetoothDisconnect>(name_static(name), args)
        }
        "system.usb.devices" => check::<SystemUsbDevices>(name_static(name), args),
        "system.battery" => check::<SystemBattery>(name_static(name), args),
        "system.network.interfaces" => check::<SystemNetworkInterfaces>(name_static(name), args),
        "system.network.routes" => check::<SystemNetworkRoutes>(name_static(name), args),
        "system.network.connections" => check::<SystemNetworkConnections>(name_static(name), args),
        "system.process.list" => check::<SystemProcessList>(name_static(name), args),
        "system.process.info" => check::<SystemProcessInfo>(name_static(name), args),
        "system.process.signal" => check::<SystemProcessSignal>(name_static(name), args),
        "system.fsevents.watch" => check::<SystemFseventsWatch>(name_static(name), args),
        "system.spotlight.query" => check::<SystemSpotlightQuery>(name_static(name), args),
        "system.metadata" => check::<SystemMetadata>(name_static(name), args),
        "app.list" => check::<AppList>(name_static(name), args),
        "app.snapshot" => check::<AppSnapshot>(name_static(name), args),
        "app.click" => check::<AppClick>(name_static(name), args),
        "app.type" => check::<AppType>(name_static(name), args),
        "app.scroll" => check::<AppScroll>(name_static(name), args),
        "app.eval" => check::<AppEval>(name_static(name), args),
        _ => Err(BridgeError::Protocol(format!("unknown tool: {name}"))),
    }
}

/// Map a runtime tool name to its canonical &'static str (used in error
/// messages). Falls back to a generic label.
fn name_static(name: &str) -> &'static str {
    for &n in TOOL_NAMES {
        if n == name {
            return n;
        }
    }
    "<unknown>"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_canonical_name_has_a_descriptor() {
        for name in TOOL_NAMES {
            assert!(find(name).is_some(), "missing descriptor for tool: {name}");
        }
        assert_eq!(list().len(), TOOL_NAMES.len());
    }

    #[test]
    fn validate_rejects_missing_required_fields() {
        let err = validate_args("tab.open", &json!({})).unwrap_err();
        match err {
            BridgeError::InvalidParams { method, .. } => assert_eq!(method, "tab.open"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_well_formed_tab_open() {
        validate_args(
            "tab.open",
            &json!({"url": "https://example.com", "wait_until": "load"}),
        )
        .unwrap();
    }

    #[test]
    fn tools_list_payload_is_stable() {
        // Golden-file pattern: catch accidental tool-surface drift (renamed
        // tools, removed descriptors, schema-shape regressions). The shape
        // we assert is byte-exact for the canonical name list and contains
        // an inputSchema for every tool.
        let v = serde_json::to_value(list()).expect("serialize");
        let arr = v.as_array().expect("array");
        for (i, t) in arr.iter().enumerate() {
            assert_eq!(
                t["name"].as_str().expect("name is string"),
                TOOL_NAMES[i],
                "tool order drift at index {i}"
            );
            assert!(t["description"].is_string(), "missing description at {i}");
            assert!(t["inputSchema"].is_object(), "missing inputSchema at {i}");
        }
        // Tools that historically returned image content must keep that path
        // alive — `into_tool_result` special-cases page.screenshot.
        assert!(arr.iter().any(|t| t["name"] == "page.screenshot"));
    }

    #[test]
    fn u2_worker_stubs_are_not_published() {
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
                !TOOL_NAMES.iter().any(|name| *name == absent),
                "worker/service-worker stub leaked into MCP tool surface: {absent}"
            );
            assert!(
                find(absent).is_none(),
                "worker/service-worker stub unexpectedly has a descriptor: {absent}"
            );
        }
    }

    #[test]
    fn validate_accepts_predicate_string_or_object() {
        validate_args(
            "tab.wait",
            &json!({"tab_id": "t1", "predicate": "networkidle"}),
        )
        .unwrap();
        validate_args(
            "tab.wait",
            &json!({"tab_id": "t1", "predicate": {"selector": "#submit"}}),
        )
        .unwrap();
    }
}
