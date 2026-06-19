//! Golden tests for `focus-manager::spawn_flags::build_argv`. Locked to
//! catch regressions in the SPEC §5 layered defense.

use focus_manager::spawn_flags::build_argv;
use focus_manager::SpawnMode;
use std::ffi::OsString;
use std::path::PathBuf;

#[test]
fn headless_argv_has_locked_invariants() {
    let argv = build_argv(SpawnMode::Headless, &PathBuf::from("/tmp/ofa-uddir"), &[]);
    let strs: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();

    // Required for SPEC D4 transport.
    assert!(strs.iter().any(|s| s == "--remote-debugging-pipe"));

    // Headless flag.
    assert!(strs.iter().any(|s| s == "--headless=new"));

    // Hygiene flags called out in SPEC §5 / §automation hygiene.
    for needed in [
        "--no-first-run",
        "--no-default-browser-check",
        "--enable-automation",
        "--disable-blink-features=AutomationControlled",
        "--use-mock-keychain",
        "--password-store=basic",
        "--mute-audio",
        "--hide-scrollbars",
    ] {
        assert!(
            strs.iter().any(|s| s == needed),
            "missing flag in headless argv: {needed}"
        );
    }

    // Headless mode MUST NOT include the headed-only flags.
    for forbidden in [
        "--window-position=-32000,-32000",
        "--no-startup-window",
        "--silent-launch",
    ] {
        assert!(
            !strs.iter().any(|s| s == forbidden),
            "headless argv unexpectedly contains: {forbidden}"
        );
    }
}

#[test]
fn headed_argv_has_offscreen_position_and_no_headless() {
    let argv = build_argv(SpawnMode::Headed, &PathBuf::from("/tmp/ofa-uddir-2"), &[]);
    let strs: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();

    // Layer B from SPEC §5: offscreen position + no startup window.
    for needed in [
        "--remote-debugging-pipe",
        "--window-position=-32000,-32000",
        "--no-startup-window",
        "--silent-launch",
    ] {
        assert!(
            strs.iter().any(|s| s == needed),
            "missing flag in headed argv: {needed}"
        );
    }

    // No headless leakage.
    assert!(!strs.iter().any(|s| s == "--headless=new"));
}

#[test]
fn extra_args_appended_after_locked_flags() {
    let extra = [
        OsString::from("--proxy-server=http://127.0.0.1:8888"),
        OsString::from("--lang=en-GB"),
    ];
    let argv = build_argv(SpawnMode::Headless, &PathBuf::from("/x"), &extra);
    assert_eq!(
        argv.last().unwrap().to_string_lossy(),
        "--lang=en-GB",
        "extra args must be appended verbatim at the end"
    );
}
