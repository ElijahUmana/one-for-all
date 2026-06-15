//! SPEC §10 — compile-time prevention of CDP method-name typos.
//!
//! The whole point of `cdp-client`'s typed bindings is that a misspelled
//! command name fails to compile, instead of producing a `-32601 Method not
//! found` at runtime. This test pins that guarantee: each `tests/ui/*.rs`
//! file is expected NOT to compile, and the matching `.stderr` snapshot
//! captures *why*. If a future refactor accidentally permits the typo to
//! compile, this test fails — proving that the runtime safety property has
//! regressed.

#[test]
fn method_typo_is_caught_at_compile_time() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/method_typo_rejected.rs");
}
