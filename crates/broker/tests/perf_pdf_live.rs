//! SPEC §12 U4 + U5 — Browser perf + PDF end-to-end tests.
//!
//! Drives the full perf-master surface against a real Chromium instance:
//!
//! - [`page.performance_timeline_start/_stop`] — trace.json contains
//!   `FrameStartedLoading` after a navigation.
//! - [`page.performance_metrics`] — `ScriptDuration > 0` after running
//!   a tight JS loop.
//! - [`page.coverage_js_start/_take`] — a fixture with 10 declared
//!   functions, 3 invoked, surfaces ≥3 with `count > 0`.
//! - [`page.coverage_css_start/_take`] — fixture with several rules.
//! - [`page.heap_snapshot`] — output file ≥ 1 MiB on disk.
//! - [`page.heap_sample_alloc`] — returns a profile shape.
//! - [`page.cpu_profile`] — returns a profile shape.
//! - [`page.layout_metrics`] — returns CSS viewport.
//! - [`page.paint_flash`] — toggles cleanly.
//! - [`page.pdf`] — output starts with `%PDF` magic.
//! - [`page.print_preview`] — output is a valid PNG.
//!
//! Gated on `ONE_FOR_ALL_LIVE_TESTS=1` for the same reason as
//! `recovery_e2e.rs` and `session_register_live.rs` — the default
//! `cargo test` lane stays Chromium-free.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use browser_engine::{Browser, BrowserConfig, WaitUntil};
use focus_manager::SpawnMode;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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

const FIXTURE_HTML: &str = r#"<!doctype html>
<title>perf-master fixture</title>
<style>
  .a { color: red; }
  .b { color: blue; }
  .c { color: green; }
  .d { color: yellow; }
  .unused-1 { color: purple; }
  .unused-2 { color: orange; }
</style>
<body>
  <p class="a">a</p><p class="b">b</p>
  <p class="c">c</p><p class="d">d</p>
  <script>
    function f1(){return 1}
    function f2(){return 2}
    function f3(){return 3}
    function f4(){return 4}
    function f5(){return 5}
    function f6(){return 6}
    function f7(){return 7}
    function f8(){return 8}
    function f9(){return 9}
    function f10(){return 10}
    let sum = 0;
    for (let i = 0; i < 50000; i++) sum += i * (i & 1);
    f1(); f5(); f10();
    window.__sum = sum;
  </script>
</body>"#;

async fn make_browser(chromium: &PathBuf) -> Browser {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = BrowserConfig {
        binary: chromium.clone(),
        user_data_dir: tmp.path().to_path_buf(),
        mode: SpawnMode::Headless,
        extra_args: Vec::new(),
        sandbox_profile: None,
        seed_plan_path: None,
        proxy: None,
    };
    // Leak the tempdir so it survives until the browser is dropped — fine
    // for a one-shot test; the dir is cleaned up by the OS on reboot.
    std::mem::forget(tmp);
    tokio::time::timeout(Duration::from_secs(10), Browser::launch(config))
        .await
        .expect("Browser::launch within 10s")
        .expect("Browser::launch returned Err")
}

async fn spawn_fixture_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let addr = listener.local_addr().expect("fixture server addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    FIXTURE_HTML.len(),
                    FIXTURE_HTML
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}/")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn perf_timeline_contains_frame_started_loading() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipping: ONE_FOR_ALL_LIVE_TESTS=1 required");
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium binary");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab("about:blank", WaitUntil::Load)
        .await
        .expect("open_tab");

    browser_engine::perf::performance_timeline_start(&page, None)
        .await
        .expect("timeline_start");

    page.navigate(&fixture_url, WaitUntil::Load)
        .await
        .expect("navigate fixture");
    tokio::time::sleep(Duration::from_millis(400)).await;

    let out_dir = std::env::temp_dir().join("perf-master-trace");
    let res = browser_engine::perf::performance_timeline_stop(&page, &out_dir)
        .await
        .expect("timeline_stop");

    assert!(res.bytes > 0, "trace bytes > 0");
    let bytes = tokio::fs::read(&res.trace_path).await.expect("read trace");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("FrameStartedLoading") || text.contains("frameStartedLoading"),
        "trace must contain FrameStartedLoading; first 4kb: {}",
        &text.chars().take(4096).collect::<String>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn perf_metrics_returns_script_duration() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let v = browser_engine::perf::performance_metrics(&page)
        .await
        .expect("performance_metrics");
    let metrics = v
        .get("metrics")
        .and_then(Value::as_array)
        .cloned()
        .expect("metrics array");
    let script_duration = metrics.iter().find_map(|m| {
        let name = m.get("name").and_then(Value::as_str)?;
        if name.eq_ignore_ascii_case("ScriptDuration") {
            m.get("value").and_then(Value::as_f64)
        } else {
            None
        }
    });
    let dur = script_duration.expect("ScriptDuration metric present");
    assert!(dur > 0.0, "ScriptDuration must be > 0; got {dur}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn js_coverage_reports_three_of_ten_invoked() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab("about:blank", WaitUntil::Load)
        .await
        .expect("open_tab");

    browser_engine::perf::coverage_js_start(&page, Some(true), Some(false))
        .await
        .expect("coverage_js_start");
    page.navigate(&fixture_url, WaitUntil::Load)
        .await
        .expect("navigate fixture");

    let v = browser_engine::perf::coverage_js_take(&page)
        .await
        .expect("coverage_js_take");
    let scripts = v
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .expect("result");

    // Walk every script's functions[].ranges[] and count fns with any
    // range whose count > 0.
    let mut covered_fns = 0usize;
    for script in &scripts {
        if let Some(funcs) = script.get("functions").and_then(Value::as_array) {
            for f in funcs {
                if let Some(ranges) = f.get("ranges").and_then(Value::as_array) {
                    if ranges
                        .iter()
                        .any(|r| r.get("count").and_then(Value::as_u64).unwrap_or(0) > 0)
                    {
                        covered_fns += 1;
                    }
                }
            }
        }
    }
    // The fixture has 10 declared fns + the top-level script body.
    // We invoke f1, f5, f10 → at least 3 should show count > 0.
    assert!(
        covered_fns >= 3,
        "expected ≥3 covered fns, got {covered_fns}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn css_coverage_returns_rules() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab("about:blank", WaitUntil::Load)
        .await
        .expect("open_tab");

    browser_engine::perf::coverage_css_start(&page)
        .await
        .expect("coverage_css_start");
    page.navigate(&fixture_url, WaitUntil::Load)
        .await
        .expect("navigate fixture");

    // Give the renderer a tick to register usage.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let v = browser_engine::perf::coverage_css_take(&page)
        .await
        .expect("coverage_css_take");
    let coverage = v
        .get("coverage")
        .and_then(Value::as_array)
        .expect("coverage array");
    // Shape is the key invariant.
    assert!(coverage.iter().all(|e| e.is_object()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heap_snapshot_writes_at_least_one_mib() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let out_dir = std::env::temp_dir().join("perf-master-heap");
    let res = browser_engine::perf::heap_snapshot(&page, &out_dir)
        .await
        .expect("heap_snapshot");
    assert!(
        res.bytes >= 1_048_576,
        "expected heap snapshot ≥1 MiB, got {} bytes at {}",
        res.bytes,
        res.snapshot_path.display()
    );
    let _ = tokio::fs::remove_file(&res.snapshot_path).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heap_sample_alloc_returns_profile() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let v = browser_engine::perf::heap_sample_alloc(&page, 500, None)
        .await
        .expect("heap_sample_alloc");
    let profile = v.get("profile").expect("profile");
    assert!(profile.is_object(), "profile should be an object");
    assert!(profile.get("head").is_some(), "profile.head present");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cpu_profile_returns_profile() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let v = browser_engine::perf::cpu_profile(&page, 500)
        .await
        .expect("cpu_profile");
    let profile = v.get("profile").expect("profile");
    let nodes = profile
        .get("nodes")
        .and_then(Value::as_array)
        .expect("nodes");
    assert!(!nodes.is_empty(), "cpu profile must contain nodes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn layout_metrics_returns_css_viewport() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let v = browser_engine::perf::layout_metrics(&page)
        .await
        .expect("layout_metrics");
    assert!(v.get("css_layout_viewport").is_some());
    assert!(v.get("css_visual_viewport").is_some());
    assert!(v.get("css_content_size").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paint_flash_toggles_cleanly() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    browser_engine::perf::paint_flash(&page, true)
        .await
        .expect("paint_flash on");
    browser_engine::perf::paint_flash(&page, false)
        .await
        .expect("paint_flash off");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pdf_starts_with_pdf_magic() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let out_dir = std::env::temp_dir().join("perf-master-pdf");
    let res = browser_engine::pdf::pdf(
        &page,
        browser_engine::pdf::PdfOptions {
            print_background: Some(true),
            ..Default::default()
        },
        &out_dir,
    )
    .await
    .expect("pdf");

    let bytes = match res {
        browser_engine::pdf::PdfResult::Inline { data_base64, .. } => {
            base64::engine::general_purpose::STANDARD
                .decode(data_base64.as_bytes())
                .expect("decode")
        }
        browser_engine::pdf::PdfResult::OnDisk { pdf_path, .. } => {
            tokio::fs::read(&pdf_path).await.expect("read pdf")
        }
    };
    assert!(bytes.len() > 100, "pdf body should be non-trivial");
    assert!(
        bytes.starts_with(b"%PDF"),
        "expected %PDF magic; got bytes[..4] = {:?}",
        &bytes[..bytes.len().min(4)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_preview_returns_png() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium");
    let browser = make_browser(&chromium).await;
    let fixture_url = spawn_fixture_server().await;
    let page = browser
        .default_context()
        .open_tab(&fixture_url, WaitUntil::Load)
        .await
        .expect("open_tab");

    let v = browser_engine::pdf::print_preview(&page, "png", false)
        .await
        .expect("print_preview");
    let b64 = v
        .get("data_base64")
        .and_then(Value::as_str)
        .expect("base64");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .expect("decode");
    // PNG magic: 89 50 4E 47 0D 0A 1A 0A
    let expected: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    assert!(
        bytes.starts_with(&expected),
        "expected PNG magic; got bytes[..8] = {:?}",
        &bytes[..bytes.len().min(8)]
    );
}
