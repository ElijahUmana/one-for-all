//! SPEC §11 V2 — Calculator e2e closeout test.
//!
//! Drives the full base-V2 stack against a real Calculator.app instance:
//! - launches Calculator if not running
//! - takes a snapshot (`AppController::snapshot`)
//! - resolves the AX ref for the "5" button by name
//! - clicks it (`AppController::click`)
//! - re-snapshots, asserts the display value contains "5"
//!
//! Gated behind `OFA_E2E_CALCULATOR=1` AND macOS so it's not noise on CI
//! runners that don't have AX permission granted to the test binary.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::Duration;

use native_control::AppController;

fn e2e_enabled() -> bool {
    std::env::var("OFA_E2E_CALCULATOR").is_ok()
}

async fn launch_calculator_if_needed() {
    let _ = Command::new("/usr/bin/open")
        .args(["-a", "Calculator"])
        .status();
    // Give it time to come up.
    tokio::time::sleep(Duration::from_millis(800)).await;
}

#[tokio::test]
async fn calculator_click_5_displays_5() {
    if !e2e_enabled() {
        eprintln!("OFA_E2E_CALCULATOR not set; skipping (set =1 to run on a Mac with AX granted)");
        return;
    }
    if !native_control::permission::is_trusted() {
        panic!("AX trust required to run e2e — grant Accessibility permission to this test binary first");
    }

    launch_calculator_if_needed().await;

    let controller = AppController::new();
    let snap = controller
        .snapshot("com.apple.calculator")
        .await
        .expect("snapshot should succeed");
    assert!(!snap.elements.is_empty(), "snapshot should have elements");

    // Find the AX button whose name is "5". Calculator buttons are AXButton
    // with `name = digit`.
    let five = snap
        .elements
        .iter()
        .find(|e| e.role == "AXButton" && e.name.trim() == "5")
        .expect("Calculator should expose a button labeled \"5\"");
    let r = five.element_ref.clone();

    controller
        .click("com.apple.calculator", &r)
        .await
        .expect("click on \"5\" should succeed");

    // Re-snapshot — display text should contain "5".
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after = controller
        .snapshot("com.apple.calculator")
        .await
        .expect("re-snapshot should succeed");

    let has_five = after.elements.iter().any(|e| {
        // Calculator's display can be exposed as AXStaticText / AXTextField
        // depending on macOS version. Check both `value` and `name`.
        e.value.as_deref().map(|v| v.contains('5')).unwrap_or(false)
            || e.name.contains('5') && e.role.contains("Text")
    });
    assert!(
        has_five,
        "Calculator display should contain '5' after clicking; got elements={:?}",
        after
            .elements
            .iter()
            .take(20)
            .map(|e| (&e.role, &e.name, &e.value))
            .collect::<Vec<_>>()
    );
}
