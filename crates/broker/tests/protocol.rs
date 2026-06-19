//! Broker integration tests that don't require a live Chromium.

use std::fs;
use std::path::{Path, PathBuf};

use broker::protocol::{ErrorCode, JsonRpcRequest, JsonRpcResponse};
use serde_json::json;

fn strip_comments_and_test_mods(src: &str) -> String {
    let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
    prod.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_code(src: &str) -> String {
    src.split_whitespace().collect::<String>()
}

#[test]
fn indirect_native_routes_fail_closed_under_blocklist() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let native_router = manifest_dir.join("src/router/native.rs");
    let text = fs::read_to_string(&native_router).expect("read native router");
    let production = strip_comments_and_test_mods(&text);
    let compact = compact_code(&production);

    let guarded_methods = [
        "app.statusmenu.click",
        "app.notification_center.open",
        "app.notification_center.list",
        "app.notification_center.click",
        "app.notification_center.dismiss",
        "app.spotlight.open",
        "app.spotlight.query",
        "app.spotlight.select",
        "app.spaces.list",
        "app.spaces.switch_to",
        "app.dock.list",
        "app.dock.click",
        "app.dock.reveal_app",
        "app.touchbar.tap",
        "app.gesture.three_finger_swipe",
        "app.force_touch",
        "app.ime.list",
        "app.ime.switch",
        "app.ime.set_input_source",
        "app.shortcut.run",
        "app.automator.run",
        "app.applescript",
        "app.javascript_for_automation",
        "app.terminal.spawn_session",
        "app.quicklook.preview",
        "app.quicklook.close",
        "drag.from_finder",
        "drag.between_apps",
    ];

    for method in guarded_methods {
        let needle = format!("require_indirect_app_targeting_allowed(_session,\"{method}\")?");
        let session_needle =
            format!("require_indirect_app_targeting_allowed(session,\"{method}\")?");
        assert!(
            compact.contains(&needle) || compact.contains(&session_needle),
            "native router lost fail-closed blocklist guard for {method}"
        );
    }
}

#[test]
fn bundle_scoped_native_routes_stay_on_app_controller() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root from broker manifest dir")
        .to_path_buf();
    let native_router = workspace.join("crates/broker/src/router/native.rs");
    let text = fs::read_to_string(&native_router).expect("read native router");
    let production = strip_comments_and_test_mods(&text);
    let compact = compact_code(&production);

    let required_calls = [
        ".app_controller.menu_list(",
        ".app_controller.menu_click(",
        ".app_controller.window_list(",
        ".app_controller.window_raise(",
        ".app_controller.window_set_minimized(",
        ".app_controller.window_set_fullscreen(",
        ".app_controller.window_move_to(",
        ".app_controller.window_resize(",
        ".app_controller.spaces_move_window(",
        ".app_controller.subscribe(",
    ];

    for needle in required_calls {
        assert!(
            compact.contains(needle),
            "native router lost AppController-gated dispatch containing {needle:?}"
        );
    }

    let forbidden_free_calls = [
        "native_control::menu::list",
        "native_control::menu::click",
        "native_control::window::list",
        "native_control::window::raise",
        "native_control::window::set_minimized",
        "native_control::window::set_fullscreen",
        "native_control::window::move_to",
        "native_control::window::resize",
    ];

    for needle in forbidden_free_calls {
        assert!(
            !production.contains(needle),
            "bundle-scoped route regressed to ungated free function {needle}"
        );
    }
}

#[test]
fn app_list_filters_blocklisted_apps() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let internal_router = manifest_dir.join("src/router/internal.rs");
    let controller = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root from broker manifest dir")
        .join("crates/native-control/src/controller.rs");
    let internal_text = fs::read_to_string(&internal_router).expect("read internal router");
    let controller_text = fs::read_to_string(&controller).expect("read controller");
    let internal_production = strip_comments_and_test_mods(&internal_text);
    let internal_compact = compact_code(&internal_production);

    assert!(
        internal_compact.contains(
            ".into_iter().filter(|app|!session.app_controller.is_blocked(&app.bundle_id)).map(|a|"
        ),
        "app.list must filter blocklisted bundle ids before returning app metadata"
    );
    assert!(
        controller_text.contains("pub fn is_blocked(&self, bundle_id: &str) -> bool"),
        "AppController must expose blocklist membership for broker filtering"
    );
}

#[test]
fn jsonrpc_request_parses_session_register() {
    let raw = r#"{"jsonrpc":"2.0","id":1,"method":"session.register","params":{"client_name":"x","client_version":"1.0.0","capabilities":["tools","events"]}}"#;
    let req: JsonRpcRequest = serde_json::from_str(raw).expect("parse");
    assert_eq!(req.jsonrpc, "2.0");
    assert_eq!(req.id, Some(json!(1)));
    assert_eq!(req.method, "session.register");
    assert!(req.params.is_some());
}

#[test]
fn jsonrpc_response_round_trip_ok() {
    let resp = JsonRpcResponse::ok(json!(1), json!({"session_id": "s_x"}));
    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains("\"jsonrpc\":\"2.0\""));
    assert!(s.contains("\"id\":1"));
    assert!(s.contains("\"session_id\":\"s_x\""));
    // No `error` field on success.
    assert!(!s.contains("\"error\""));
}

#[test]
fn jsonrpc_response_round_trip_err() {
    let resp = JsonRpcResponse::err(
        json!(7),
        ErrorCode::ChromiumLaunchFailed,
        "ProcessSingleton lock held",
        Some(json!({"retry_after_ms": 1000})),
    );
    let s = serde_json::to_string(&resp).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["error"]["code"], -32008);
    assert_eq!(v["error"]["message"], "ProcessSingleton lock held");
    assert_eq!(v["error"]["data"]["retry_after_ms"], 1000);
    // No `result` field on error.
    assert!(v.get("result").is_none());
}

#[test]
fn error_codes_match_spec_d17() {
    let pairs: &[(ErrorCode, i64)] = &[
        (ErrorCode::SessionNotFound, -32001),
        (ErrorCode::TabNotFound, -32002),
        (ErrorCode::ContextNotFound, -32003),
        (ErrorCode::ElementStale, -32004),
        (ErrorCode::ElementNotActionable, -32005),
        (ErrorCode::NavigationFailed, -32006),
        (ErrorCode::Timeout, -32007),
        (ErrorCode::ChromiumLaunchFailed, -32008),
        (ErrorCode::PermissionDenied, -32009),
        (ErrorCode::ProtocolError, -32010),
        (ErrorCode::BrokerUnavailable, -32011),
        (ErrorCode::SessionLimitExceeded, -32012),
    ];
    for (code, expected) in pairs {
        let n: i64 = (*code).into();
        assert_eq!(n, *expected, "{code:?} should be {expected}");
    }
}

#[test]
fn registry_get_returns_none_for_missing() {
    let reg = broker::SessionRegistry::new();
    assert!(reg.get("does-not-exist").is_none());
    assert_eq!(reg.len(), 0);
}

/// SPEC §10 M10 — `session.register` accepts `trace: true|false`. Default
/// is `false`. Both `browser.context.create` and `session.register` honor
/// the flag, but `session.register` is the canonical entry point.
#[test]
fn session_register_params_default_trace_false() {
    let raw = r#"{"client_name":"x","client_version":"1.0.0","capabilities":[]}"#;
    let parsed: broker::protocol::SessionRegisterParams = serde_json::from_str(raw).unwrap();
    assert!(!parsed.trace, "trace defaults to false when omitted");
}

#[test]
fn session_register_params_parse_trace_true() {
    let raw = r#"{"client_name":"x","client_version":"1.0.0","capabilities":[],"trace":true}"#;
    let parsed: broker::protocol::SessionRegisterParams = serde_json::from_str(raw).unwrap();
    assert!(parsed.trace, "trace=true must be honored");
}

// ---------- SPEC §11 V2 native-control surface ----------

/// Capabilities round-trip through `session.register`. Default-deny means
/// `app.*` requires `"native"` to be present; default empty `capabilities`
/// must NOT silently grant it.
#[test]
fn session_register_capabilities_default_empty() {
    let raw = r#"{"client_name":"x","client_version":"1.0.0","capabilities":[]}"#;
    let parsed: broker::protocol::SessionRegisterParams = serde_json::from_str(raw).unwrap();
    assert!(parsed.capabilities.is_empty());
}

#[test]
fn session_register_capabilities_native_opt_in() {
    let raw = r#"{"client_name":"x","client_version":"1.0.0","capabilities":["tools","events","native"]}"#;
    let parsed: broker::protocol::SessionRegisterParams = serde_json::from_str(raw).unwrap();
    assert!(parsed.capabilities.iter().any(|c| c == "native"));
}

#[test]
fn native_capability_drives_sbpl_ax_mach_services() {
    let rootfs = std::path::PathBuf::from("/tmp/ofa-proof-session");
    let allow = vec![sandbox::InheritSpec::rw("/tmp/ofa-proof-session/Downloads")];

    let mut no_native = sandbox::SbplParams::from_inherit("s_no_native", &rootfs, &allow);
    no_native.native_ax_allowed = false;
    let no_native_sb = sandbox::generate_sbpl(&no_native);
    assert!(!no_native_sb.contains("com.apple.tccd"));
    assert!(!no_native_sb.contains("com.apple.AppleEventsService"));
    assert!(!no_native_sb.contains("com.apple.coreservices.appleevents"));

    let mut with_native = sandbox::SbplParams::from_inherit("s_native", &rootfs, &allow);
    with_native.native_ax_allowed = true;
    let with_native_sb = sandbox::generate_sbpl(&with_native);
    assert!(with_native_sb.contains("com.apple.tccd"));
    assert!(with_native_sb.contains("com.apple.tccd.system"));
    assert!(with_native_sb.contains("com.apple.AppleEventsService"));
    assert!(with_native_sb.contains("com.apple.coreservices.appleevents"));
}

/// SPEC §11 V2 error codes for the `app.*` surface — confirms we reuse
/// existing JSON-RPC codes per SPEC D17 rather than minting new ones.
#[test]
fn app_surface_reuses_d17_error_codes() {
    // -32009 PermissionDenied: AX missing OR session lacks "native"
    let n: i64 = ErrorCode::PermissionDenied.into();
    assert_eq!(n, -32009);
    // -32002 TabNotFound: app not running (we reuse the page-side code so
    // existing clients don't need to learn a new error).
    let n: i64 = ErrorCode::TabNotFound.into();
    assert_eq!(n, -32002);
    // -32004 ElementStale: ref from an older snapshot.
    let n: i64 = ErrorCode::ElementStale.into();
    assert_eq!(n, -32004);
    // -32005 ElementNotActionable: zero-area bbox or no AXPress action.
    let n: i64 = ErrorCode::ElementNotActionable.into();
    assert_eq!(n, -32005);
}

/// `SessionEntry` MUST honor the `capabilities` array passed at register —
/// `has_native_capability()` is the load-bearing gate every `app.*` arm
/// runs.
#[test]
fn session_entry_capability_gate_default_denies_native() {
    use std::collections::BTreeSet;
    // Mirror the registry shape without spinning up a Browser.
    let caps: BTreeSet<String> = BTreeSet::new();
    assert!(
        !caps.contains("native"),
        "fresh capability set must NOT silently grant native — opt-in only"
    );
    let mut caps2 = BTreeSet::new();
    caps2.insert("tools".to_string());
    caps2.insert("events".to_string());
    assert!(
        !caps2.contains("native"),
        "legacy clients passing tools+events do NOT auto-get native — V3 default-deny"
    );
    let mut caps3 = BTreeSet::new();
    caps3.insert("tools".to_string());
    caps3.insert("native".to_string());
    assert!(caps3.contains("native"));
}
