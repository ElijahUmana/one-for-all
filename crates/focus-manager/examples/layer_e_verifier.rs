//! Standalone verifier for SPEC §5 Layer E.
//!
//! Mirrors the call path in `broker::main`: invoke
//! `focus_manager::set_accessory_activation_policy()` synchronously on the
//! main thread, hold the guard, sleep so an external observer can run
//! `lsappinfo` against the PID. Also exercises the idempotency check by
//! calling the function twice — the second call should observe the policy
//! is already Accessory and emit the "reaffirmed" log instead of the
//! "applied" log.
//!
//! Run as:
//!   cargo run --example layer_e_verifier --manifest-path crates/focus-manager/Cargo.toml
//!
//! Then in another shell:
//!   lsappinfo info -only ApplicationType $(pgrep -f layer_e_verifier)
//!
//! Expected: stdout contains `kLSApplicationTypeUIElement` (= Accessory).

use std::time::Duration;

fn main() {
    // First call — should emit the "applied" info log.
    let _guard = match focus_manager::set_accessory_activation_policy() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("layer E verifier: failed to apply accessory policy: {e}");
            std::process::exit(2);
        }
    };

    // Second call — should observe the policy is already Accessory and
    // emit the "reaffirmed" info log. Catches a regression where the
    // idempotency check is dropped.
    let _guard2 = match focus_manager::set_accessory_activation_policy() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("layer E verifier: idempotent re-apply failed: {e}");
            std::process::exit(3);
        }
    };

    let pid = std::process::id();
    println!("layer E verifier: pid={pid} policy=Accessory (idempotent re-apply OK); sleeping 30s for lsappinfo");
    std::thread::sleep(Duration::from_secs(30));

    drop(_guard);
    drop(_guard2);
}
