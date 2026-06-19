//! Exact Chromium flag lists per [`SpawnMode`].
//!
//! Every flag is documented with its *purpose*, not just its name. New flags
//! must be reviewed against the SPEC's "automation hygiene" section.

use std::ffi::OsString;
use std::path::Path;

use crate::SpawnMode;

/// Build the full Chromium argv for the given mode. The return value does NOT
/// include the binary path itself — the caller passes that to `Command::new`.
pub fn build_argv(mode: SpawnMode, user_data_dir: &Path, extra: &[OsString]) -> Vec<OsString> {
    let mut v: Vec<OsString> = Vec::with_capacity(48 + extra.len());

    // Common flags — apply in both modes.
    v.push("--remote-debugging-pipe".into()); // CDP on fd 3/4 NUL-delimited JSON, NOT websocket
    v.push(format!("--user-data-dir={}", user_data_dir.display()).into());
    v.push("--no-first-run".into());
    v.push("--no-default-browser-check".into());
    v.push(
        "--disable-features=Translate,OptimizationHints,MediaRouter,InterestFeedContentSuggestions"
            .into(),
    );
    v.push("--password-store=basic".into()); // suppress macOS Keychain prompt
    v.push("--use-mock-keychain".into());
    v.push("--disable-background-networking".into());
    v.push("--disable-component-update".into());
    v.push("--disable-default-apps".into());
    v.push("--disable-sync".into());
    v.push("--metrics-recording-only".into());
    v.push("--no-pings".into());
    v.push("--enable-automation".into());
    // Tame Blink's automation flag so navigator.webdriver is not trivially set.
    v.push("--disable-blink-features=AutomationControlled".into());
    // Stop Chrome from registering itself as the default app, opening Welcome
    // tabs, or annoying the user in any other way.
    v.push("--disable-prompt-on-repost".into());
    v.push("--disable-hang-monitor".into());
    v.push("--disable-popup-blocking".into());
    v.push("--disable-client-side-phishing-detection".into());
    v.push("--disable-domain-reliability".into());
    v.push("--disable-renderer-backgrounding".into());
    v.push("--disable-backgrounding-occluded-windows".into());
    v.push("--disable-ipc-flooding-protection".into());
    v.push("--force-color-profile=srgb".into());

    match mode {
        SpawnMode::Headless => {
            v.push("--headless=new".into());
            v.push("--hide-scrollbars".into());
            v.push("--mute-audio".into());
            v.push("--disable-gpu".into()); // Avoid GPU init overhead when truly headless on macOS
        }
        SpawnMode::Headed => {
            // Offscreen until raised — a guard for the rare case where focus
            // restoration in macos.rs lags behind the OS window-create event.
            v.push("--window-position=-32000,-32000".into());
            v.push("--window-size=1280,800".into());
            // Don't open a startup window; broker will create tabs via
            // Target.createTarget.
            v.push("--no-startup-window".into());
            // Quiets the extra noise that NSWorkspace produces on launch.
            v.push("--silent-launch".into());
        }
    }

    // Caller-supplied tail (rarely used; broker uses defaults).
    v.extend_from_slice(extra);

    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn user_data_dir_is_inlined() {
        let dir = PathBuf::from("/tmp/abc xyz/with space");
        let v = build_argv(SpawnMode::Headless, &dir, &[]);
        let arg = v
            .iter()
            .find(|a| a.to_string_lossy().starts_with("--user-data-dir="))
            .expect("flag present");
        assert_eq!(
            arg.to_string_lossy(),
            "--user-data-dir=/tmp/abc xyz/with space"
        );
    }

    #[test]
    fn extra_args_are_appended_last() {
        let v = build_argv(
            SpawnMode::Headless,
            &PathBuf::from("/x"),
            &[OsString::from("--proxy-server=http://1.2.3.4:8080")],
        );
        assert_eq!(
            v.last().unwrap().to_string_lossy(),
            "--proxy-server=http://1.2.3.4:8080"
        );
    }

    #[test]
    fn headed_does_not_include_headless_flag() {
        let v = build_argv(SpawnMode::Headed, &PathBuf::from("/x"), &[]);
        assert!(!v.iter().any(|a| a == "--headless=new"));
    }
}
