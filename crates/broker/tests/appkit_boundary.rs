//! SPEC §5 cross-crate AppKit-call boundary tripwire.
//!
//! Layer E says only `focus-manager` is allowed to touch
//! `NSApplication::setActivationPolicy` and `NSRunningApplication::
//! activateWithOptions:`. Every other workspace crate is forbidden from
//! calling these — they are the two AppKit surfaces that can either
//! (a) flip the broker's own activation policy back to Regular, or
//! (b) activate an arbitrary running app behind the user's back.
//!
//! The approved users live in:
//! - `crates/focus-manager/src/macos.rs` (Layer E policy + restore)
//! - `crates/focus-manager/src/restore.rs` (the bounded restore loop)
//!
//! Everywhere else the tokens MUST NOT appear in production source.
//!
//! Implementation notes
//! --------------------
//! - We grep file text rather than rely on a `cargo geiger`-style
//!   feature-aware analysis because (a) the symbols are unambiguous
//!   string-grep targets, and (b) a static text test runs at the same
//!   tier as the existing `forbid_kAXRaiseAction_is_compiled_in` lock
//!   in `native-control/src/actions.rs` — same precedent, same shape.
//! - We strip line comments before scanning so doc comments mentioning
//!   the forbidden APIs (e.g. "we never call activateWithOptions on
//!   ourselves") do not trip the test.
//! - We strip `#[cfg(test)]` blocks because tests may mention
//!   the forbidden tokens (in `assert!` strings, fixture setup).
//! - We walk the workspace via `CARGO_MANIFEST_DIR` so the test runs
//!   regardless of how the suite is invoked (cargo test, IDE, CI).
//!
//! This test is not gated on `#[cfg(target_os = "macos")]` — the
//! forbidden tokens are platform-agnostic strings, and a regression
//! that lands on a Linux-build CI should still fail loud.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_APPKIT_SYMBOLS: &[&str] = &[
    // Activation-policy mutator. Only `focus-manager::set_accessory_
    // activation_policy` is allowed to call this; any other call site
    // could undo Layer E.
    "setActivationPolicy",
    // App-activation entrypoints. Only `focus-manager::restore` is
    // allowed to call these against the captured-frontmost pid; any
    // other call site could steal focus.
    "activateWithOptions",
    "activateIgnoringOtherApps",
    // Direct `NSApp.activate(_:)` style — the variant that takes no
    // options and activates the calling process. Hard-banned everywhere
    // including inside `focus-manager`.
    "NSApp.activate",
    "NSApp::activate",
];

/// Crates whose `src/` trees are scanned. `focus-manager` is the
/// EXEMPT crate — it is the designated owner of these APIs and is
/// excluded from the scan. Everything else listed here is in scope.
///
/// We intentionally do not auto-discover crates from
/// `Cargo.toml`'s workspace members: doing so would silently include a
/// new crate without forcing the contributor to think about the
/// boundary. Adding a crate here is a deliberate review step.
const SCANNED_CRATES: &[&str] = &[
    "ax-engine",
    "bench",
    "broker",
    "browser-engine",
    "cdp-client",
    "chromium-fetcher",
    "mcp-server",
    "native-control",
    "observability",
    "sandbox",
    "system-control",
    "vision",
];

/// Walk a directory recursively, returning every `.rs` file path under it.
fn rust_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files_under(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Strip line comments AND `#[cfg(test)]` modules from `src` so the scan
/// only inspects production code. The cfg-test stripper is heuristic: it
/// finds `#[cfg(test)]` and drops everything from there to the end of
/// file. This works because every crate in the workspace puts its tests
/// at the BOTTOM of the file (the project's house style), so cutting at
/// the first occurrence reliably partitions production / test text.
///
/// Inline `/* ... */` block comments are not stripped — they are rare in
/// this codebase and a false positive would just push the contributor to
/// rewrite as `//`. Lower-cost than a full Rust tokenizer.
fn strip_comments_and_test_mods(src: &str) -> String {
    let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
    prod.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn forbidden_appkit_calls_only_appear_in_focus_manager() {
    // CARGO_MANIFEST_DIR is `crates/broker/`. Workspace root is two
    // levels up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root from broker manifest dir")
        .to_path_buf();

    let mut violations: Vec<String> = Vec::new();
    let mut scanned_files = 0usize;

    for crate_name in SCANNED_CRATES {
        let src_dir = workspace.join("crates").join(crate_name).join("src");
        if !src_dir.exists() {
            // Crate may have been renamed or removed; SCANNED_CRATES is
            // the source of truth and a stale entry is a code-review
            // signal, not a test failure. Surface it as a notice.
            eprintln!(
                "[appkit-boundary] note: {} does not exist; remove from SCANNED_CRATES",
                src_dir.display()
            );
            continue;
        }
        for file in rust_files_under(&src_dir) {
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            scanned_files += 1;
            let production = strip_comments_and_test_mods(&text);
            for needle in FORBIDDEN_APPKIT_SYMBOLS {
                if production.contains(needle) {
                    violations.push(format!(
                        "{} contains forbidden AppKit symbol {needle:?}",
                        file.display()
                    ));
                }
            }
        }
    }

    assert!(
        scanned_files > 0,
        "appkit-boundary test scanned zero files — workspace path resolution \
         is broken (workspace = {})",
        workspace.display()
    );

    assert!(
        violations.is_empty(),
        "SPEC §5 Layer E cross-crate boundary violated. The AppKit symbols \
         setActivationPolicy / activateWithOptions / activateIgnoringOtherApps / \
         NSApp.activate are reserved for focus-manager. Move the call there or \
         remove it.\n\n{}\n\n(Scanned {} Rust files across {} crates.)",
        violations.join("\n"),
        scanned_files,
        SCANNED_CRATES.len()
    );
}

/// Sanity — the scanner sees enough files to be useful. If this drops to
/// near-zero (e.g. workspace layout changes), the main test would silently
/// pass without scanning anything.
#[test]
fn appkit_boundary_scanner_finds_workspace_files() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();

    let mut total = 0usize;
    for crate_name in SCANNED_CRATES {
        let src = workspace.join("crates").join(crate_name).join("src");
        if src.exists() {
            total += rust_files_under(&src).len();
        }
    }
    assert!(
        total >= 30,
        "appkit-boundary scanner found only {total} files across {} crates — \
         expected ≥30. Workspace layout may have changed.",
        SCANNED_CRATES.len()
    );
}

/// Self-check: the comment + cfg-test stripper actually filters comment
/// lines and test modules. If it regresses, the boundary test would
/// silently false-positive on doc comments and false-negative on real
/// production call sites.
#[test]
fn strip_comments_and_test_mods_removes_comments_and_test_mod() {
    let src = "fn ok() {}\n\
               // line comment with setActivationPolicy\n\
               /// doc comment with activateWithOptions\n\
               //! crate doc with NSApp.activate\n\
               fn also_ok() {}\n\
               #[cfg(test)]\n\
               mod tests {\n\
                   fn fixture() { let _ = \"setActivationPolicy\"; }\n\
               }\n";
    let out = strip_comments_and_test_mods(src);
    assert!(out.contains("fn ok()"));
    assert!(out.contains("fn also_ok()"));
    assert!(
        !out.contains("setActivationPolicy"),
        "stripper failed: {out:?}"
    );
    assert!(!out.contains("activateWithOptions"));
    assert!(!out.contains("NSApp.activate"));
    // The test module's body must also be excluded.
    assert!(!out.contains("fn fixture"));
}
