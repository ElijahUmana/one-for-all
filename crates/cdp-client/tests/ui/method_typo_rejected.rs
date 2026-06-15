// `NavigatParams` (with the trailing `e` missing) does not exist in
// `cdp_client::generated::domains::page` — the real type is `NavigateParams`.
// This file MUST fail to compile, which proves that misspelling a CDP
// method name is a compile-time error rather than a runtime `-32601`.
//
// `trybuild` runs this file as a separate compilation and asserts the
// resulting diagnostics match the snapshot in `method_typo_rejected.stderr`.

fn typo_must_not_compile() {
    let _: cdp_client::generated::domains::page::NavigatParams =
        cdp_client::generated::domains::page::NavigatParams {
            url: "https://example.com".to_owned(),
            ..Default::default()
        };
}

fn main() {
    typo_must_not_compile();
}
