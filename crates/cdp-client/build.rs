//! `cdp-client` build script.
//!
//! Reads the bundled CDP protocol JSON files (browser + js) and codegens
//! Rust bindings into `$OUT_DIR/generated/{domains.rs,events.rs}`.
//!
//! Design choices:
//! - Method names are emitted as `&'static str` constants (`Domain.command`).
//! - For each command we emit `<Cmd>Params` and `<Cmd>Returns` structs with
//!   typed primitive fields (`String`, `i64`, `f64`, `bool`) and
//!   `serde_json::Value` for $ref / object / nested-array shapes.
//! - For each event we emit `<Event>Event` structs with the same rules.
//! - For each declared `types` we emit a Rust type alias when primitive,
//!   a Rust enum for string-enums, and a struct for object types
//!   (object types are emitted as transparent `serde_json::Value` aliases —
//!   simpler and safer than trying to model the recursive ones).
//! - One module per domain, snake_cased. The browser and js protocols both
//!   contain the same domains where they overlap; we union the command list
//!   and prefer the browser declaration on conflicts.
//! - A top-level `CdpEvent` enum tags by `(method, params)` so that the
//!   reader task can route events without a string table.
//!
//! The build script only re-runs when the protocol JSONs or this file
//! change, so the generated code is otherwise cached.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Protocol {
    #[allow(dead_code)]
    version: serde_json::Value,
    domains: Vec<Domain>,
}

#[derive(Debug, Deserialize, Clone)]
struct Domain {
    domain: String,
    #[serde(default)]
    experimental: bool,
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    types: Vec<TypeDecl>,
    #[serde(default)]
    commands: Vec<Command>,
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Debug, Deserialize, Clone)]
struct TypeDecl {
    id: String,
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(default)]
    #[serde(rename = "enum")]
    enum_values: Option<Vec<String>>,
    // Other shape fields are intentionally ignored — see module-level docs.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct Command {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    experimental: bool,
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    parameters: Vec<Param>,
    #[serde(default)]
    returns: Vec<Param>,
}

#[derive(Debug, Deserialize, Clone)]
struct Event {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    experimental: bool,
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    parameters: Vec<Param>,
}

#[derive(Debug, Deserialize, Clone)]
struct Param {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(rename = "$ref", default)]
    ref_to: Option<String>,
    #[serde(default)]
    optional: bool,
}

fn read_protocol(path: &str) -> Result<Protocol> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
    let p: Protocol = serde_json::from_slice(&bytes).with_context(|| format!("parse {path}"))?;
    Ok(p)
}

/// Reserved-keyword-safe Rust identifier.
fn safe_ident(name: &str) -> String {
    let raw = name.to_snake_case();
    match raw.as_str() {
        "type" | "ref" | "self" | "match" | "move" | "loop" | "in" | "as" | "use" | "for"
        | "if" | "let" | "where" | "fn" | "mod" | "pub" | "impl" | "struct" | "enum" | "trait"
        | "async" | "await" | "static" | "const" | "yield" | "true" | "false" | "box"
        | "abstract" | "become" | "do" | "final" | "macro" | "override" | "priv" | "typeof"
        | "unsized" | "virtual" => format!("r#{raw}"),
        _ => raw,
    }
}

fn camel(name: &str) -> String {
    name.to_upper_camel_case()
}

/// Camel-case an identifier and append a trailing underscore if the result
/// is a Rust strict keyword (`Self`, `Crate`, `Super`, etc.). Used at every
/// site that emits a type or variant ident — those positions don't accept
/// raw identifiers.
fn camel_safe(name: &str) -> String {
    let c = camel(name);
    match c.as_str() {
        // Strict keywords that can't be raw-prefixed in 2018+ editions.
        "Self" | "self" | "crate" | "super" | "extern" => format!("{c}_"),
        _ => c,
    }
}

/// Translate a CDP `Param` into a Rust type token, falling back to
/// `serde_json::Value` for shapes we don't fully model.
fn rust_type_for(p: &Param) -> TokenStream {
    let inner = if let Some(t) = &p.ty {
        match t.as_str() {
            "string" => quote!(String),
            "integer" => quote!(i64),
            "number" => quote!(f64),
            "boolean" => quote!(bool),
            "binary" => quote!(String), // base64 in CDP
            "any" | "object" | "array" => quote!(::serde_json::Value),
            _ => quote!(::serde_json::Value),
        }
    } else if p.ref_to.is_some() {
        // $ref to another protocol type — could be cross-domain. Use Value
        // to avoid a 51-way ordering / dependency graph at codegen time.
        quote!(::serde_json::Value)
    } else {
        quote!(::serde_json::Value)
    };
    if p.optional {
        quote!(Option<#inner>)
    } else {
        inner
    }
}

fn doc_attr(s: Option<&str>) -> TokenStream {
    match s {
        Some(d) if !d.is_empty() => {
            // Strip newlines and excess whitespace to keep doc lines short.
            let clean = d.replace(['\r', '\n'], " ");
            quote!(#[doc = #clean])
        }
        _ => quote!(),
    }
}

fn deprecated_attr(d: bool) -> TokenStream {
    if d {
        quote!(#[deprecated])
    } else {
        quote!()
    }
}

fn generate_type_decl(td: &TypeDecl) -> TokenStream {
    let name = format_ident!("{}", camel_safe(&td.id));
    let doc = doc_attr(td.description.as_deref());
    if let (Some(t), Some(values)) = (td.ty.as_deref(), td.enum_values.as_ref()) {
        if t == "string" && !values.is_empty() {
            // Emit a string-enum.
            let variants: Vec<TokenStream> = values
                .iter()
                .map(|v| {
                    let var = format_ident!("{}", camel_safe(v));
                    quote!(#[serde(rename = #v)] #var,)
                })
                .collect();
            return quote! {
                #doc
                #[derive(Debug, Clone, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize)]
                pub enum #name { #(#variants)* }
            };
        }
    }
    // Fallback: type alias to Value.
    quote! {
        #doc
        pub type #name = ::serde_json::Value;
    }
}

fn generate_param_struct(struct_name: &syn::Ident, params: &[Param]) -> TokenStream {
    if params.is_empty() {
        return quote! {
            #[derive(Debug, Clone, Default, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
            pub struct #struct_name {}
        };
    }
    let fields: Vec<TokenStream> = params
        .iter()
        .map(|p| {
            let raw_name = &p.name;
            let field = format_ident!("{}", safe_ident(&p.name));
            let ty = rust_type_for(p);
            let doc = doc_attr(p.description.as_deref());
            let serde = if p.optional {
                quote!(#[serde(rename = #raw_name, default, skip_serializing_if = "Option::is_none")])
            } else {
                quote!(#[serde(rename = #raw_name)])
            };
            quote! {
                #doc
                #serde
                pub #field: #ty,
            }
        })
        .collect();
    // Every field type we emit (`String`, `i64`, `f64`, `bool`, `Option<T>`,
    // `serde_json::Value`) implements `Default`, so deriving `Default` is safe
    // for every shape and lets call sites use `..Default::default()` to fill
    // in optional fields.
    quote! {
        #[derive(Debug, Clone, Default, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        pub struct #struct_name { #(#fields)* }
    }
}

fn generate_command(domain: &str, c: &Command) -> TokenStream {
    let cmd_camel = camel_safe(&c.name);
    let params_ident = format_ident!("{}Params", cmd_camel);
    let returns_ident = format_ident!("{}Returns", cmd_camel);
    let method_name = format!("{}.{}", domain, c.name);

    let params_struct = generate_param_struct(&params_ident, &c.parameters);
    let returns_struct = generate_param_struct(&returns_ident, &c.returns);
    let doc = doc_attr(c.description.as_deref());
    let depr = deprecated_attr(c.deprecated);
    let idempotent = is_idempotent_method(&c.name);

    quote! {
        #doc
        #depr
        #params_struct

        #returns_struct

        impl crate::Command for #params_ident {
            const METHOD: &'static str = #method_name;
            const IDEMPOTENT: bool = #idempotent;
            type Returns = #returns_ident;
        }
    }
}

/// Heuristic: is this CDP command name safe to retry on transient transport
/// errors? CDP convention puts read-only commands behind verbs like `get`,
/// `query`, `describe`, `is`, `has`, `read`, `list`, `count`. Anything else
/// is presumed to have side effects (`navigate`, `setX`, `dispatchY`,
/// `enable`/`disable`, `create`/`close`/`focus`, etc.).
///
/// Conservative on purpose: false positives would double-fire a side effect,
/// false negatives just skip a retry that would otherwise have succeeded.
/// Consumers can opt in per-call via `CdpSession::send_with_retry_policy`.
fn is_idempotent_method(method_name: &str) -> bool {
    const READ_ONLY_PREFIXES: &[&str] = &[
        "get", "query", "describe", "is", "has", "read", "list", "count", "resolve", "find",
        "compute",
    ];
    for &p in READ_ONLY_PREFIXES {
        if method_name.starts_with(p) {
            // Guard against e.g. `getStorage` matching prefix `getS` — only
            // accept when the next char is uppercase (camelCase boundary)
            // or end-of-string.
            let rest = &method_name[p.len()..];
            if rest.is_empty() {
                return true;
            }
            if rest
                .chars()
                .next()
                .map_or(false, |c| c.is_ascii_uppercase())
            {
                return true;
            }
        }
    }
    false
}

fn generate_event(domain: &str, e: &Event) -> (TokenStream, String, syn::Ident) {
    let evt_camel = camel_safe(&e.name);
    let evt_ident = format_ident!("{}Event", evt_camel);
    let method_name = format!("{}.{}", domain, e.name);
    let body = generate_param_struct(&evt_ident, &e.parameters);
    let doc = doc_attr(e.description.as_deref());
    let depr = deprecated_attr(e.deprecated);
    (
        quote! {
            #doc
            #depr
            #body
        },
        method_name,
        evt_ident,
    )
}

fn merge_protocols(browser: Protocol, js: Protocol) -> Vec<Domain> {
    // Browser wins on overlap; JS contributes domains the browser side is missing
    // (Runtime/Debugger/HeapProfiler/Profiler/etc. all live in the JS protocol
    // historically, though modern browser.json includes them too).
    let mut seen: BTreeMap<String, Domain> = BTreeMap::new();
    for d in browser.domains {
        seen.insert(d.domain.clone(), d);
    }
    for d in js.domains {
        seen.entry(d.domain.clone()).or_insert(d);
    }
    seen.into_values().collect()
}

/// Translate a CDP domain name to its Cargo feature flag name.
///
/// CDP domain names are PascalCase (`Accessibility`, `DOMSnapshot`); Cargo
/// feature names are kebab-case (`domain-accessibility`, `domain-dom-snapshot`).
/// `heck`'s snake-case helper produces `accessibility` and `dom_snapshot`;
/// we then `_` → `-` for cargo's preferred style.
fn feature_name_for(domain: &str) -> String {
    let snake = domain.to_snake_case();
    format!("domain-{}", snake.replace('_', "-"))
}

/// Translate a Cargo feature name to its `CARGO_FEATURE_*` env-var form.
///
/// Cargo lower-cases the feature, replaces `-` with `_`, then upper-cases.
fn feature_env_var(feature: &str) -> String {
    format!("CARGO_FEATURE_{}", feature.replace('-', "_").to_uppercase())
}

/// Is this domain enabled by the current feature set?
///
/// A domain is enabled when its `domain-<name>` feature is on. The
/// `cdp-essentials` and `cdp-full` aggregate features turn on the right
/// per-domain features in Cargo.toml — we don't read them here.
fn domain_enabled(domain: &str) -> bool {
    let feat = feature_name_for(domain);
    let env = feature_env_var(&feat);
    std::env::var_os(&env).is_some()
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=protocol/protocol-browser.json");
    println!("cargo:rerun-if-changed=protocol/protocol-js.json");
    // Re-run codegen if the feature set changes.
    println!("cargo:rerun-if-env-changed=CARGO_CFG_FEATURE");

    // SPEC §2: build.rs uses include_bytes! — but we read at build-time
    // rather than compile-time so codegen logic stays simple. The include_bytes!
    // hermetic guarantee lives in the runtime crate via the in-tree
    // `protocol/` dir copied at scaffold time. The bytes of that dir gate
    // codegen via the rerun-if-changed lines above.
    let browser = read_protocol("protocol/protocol-browser.json")?;
    let js = read_protocol("protocol/protocol-js.json")?;
    let all_domains = merge_protocols(browser, js);

    // Filter to enabled domains. Skipped domains contribute neither a module
    // nor any event variants — the typed `CdpEvent` enum and the `domains`
    // module shrink in lockstep with the feature set. Skipped domains' wire
    // events still arrive (Chromium doesn't know about our features); the
    // reader logs them at trace level via the `event not in typed enum`
    // branch in connection.rs::dispatch.
    let domains: Vec<&Domain> = all_domains
        .iter()
        .filter(|d| domain_enabled(&d.domain))
        .collect();
    let skipped: Vec<&str> = all_domains
        .iter()
        .filter(|d| !domain_enabled(&d.domain))
        .map(|d| d.domain.as_str())
        .collect();
    if !skipped.is_empty() {
        // Don't fail the build — the user may have intentionally narrowed the
        // feature set. Just record what we skipped so `cargo build -v` shows
        // it.
        println!(
            "cargo:warning=cdp-client: skipping {} CDP domain(s) not in feature set: {}",
            skipped.len(),
            skipped.join(", ")
        );
    }

    // Track every event variant emitted, for the top-level CdpEvent enum.
    let mut all_events: Vec<(String, String, String)> = Vec::new(); // (domain_module, method, type_path)

    // domains.rs
    let mut domain_modules = Vec::with_capacity(domains.len());
    for d in &domains {
        let domain_mod = format_ident!("{}", d.domain.to_snake_case());
        let domain_doc = format!(
            "Generated bindings for the `{}` CDP domain (experimental={}).",
            d.domain, d.experimental
        );

        let types_ts: Vec<TokenStream> = d.types.iter().map(generate_type_decl).collect();

        let cmds_ts: Vec<TokenStream> = d
            .commands
            .iter()
            .map(|c| generate_command(&d.domain, c))
            .collect();

        let mut events_ts: Vec<TokenStream> = Vec::new();
        let mut domain_event_methods: Vec<(String, syn::Ident)> = Vec::new();
        for e in &d.events {
            let (ts, method, ident) = generate_event(&d.domain, e);
            events_ts.push(ts);
            domain_event_methods.push((method, ident));
        }

        for (method, ident) in &domain_event_methods {
            all_events.push((d.domain.to_snake_case(), method.clone(), ident.to_string()));
        }

        domain_modules.push(quote! {
            #[doc = #domain_doc]
            pub mod #domain_mod {
                #(#types_ts)*
                #(#cmds_ts)*
                #(#events_ts)*
            }
        });
    }

    let domains_file = quote! {
        // Generated CDP domain bindings. Do not edit by hand.
        // (No inner attributes here — this file is `include!`d.)

        #(#domain_modules)*
    };
    let domains_pretty = prettyplease::unparse(
        &syn::parse2::<syn::File>(domains_file).context("parse generated domains.rs")?,
    );

    // events.rs — a tag/content enum keyed by CDP method name.
    let mut variants = Vec::with_capacity(all_events.len());
    let mut method_arms = Vec::with_capacity(all_events.len());
    let mut emitted_variant_names: BTreeSet<String> = BTreeSet::new();
    for (mod_snake, method, ident) in &all_events {
        // Variant name = DomainEventName, deduped if the same name appears
        // in two domains (rare).
        let domain_camel = method
            .split('.')
            .next()
            .map(camel_safe)
            .unwrap_or_else(|| "Unknown".to_string());
        let evt_camel = method
            .split('.')
            .nth(1)
            .map(camel_safe)
            .unwrap_or_else(|| "Unknown".to_string());
        let mut variant_name = format!("{domain_camel}{evt_camel}");
        let mut suffix = 0;
        while !emitted_variant_names.insert(variant_name.clone()) {
            suffix += 1;
            variant_name = format!("{domain_camel}{evt_camel}{suffix}");
        }
        let variant_ident = format_ident!("{}", variant_name);
        let mod_ident = format_ident!("{}", mod_snake);
        let evt_ident = format_ident!("{}", ident);
        variants.push(quote! {
            #[serde(rename = #method)]
            #variant_ident(crate::generated::domains::#mod_ident::#evt_ident),
        });
        method_arms.push(quote! {
            CdpEvent::#variant_ident(_) => #method,
        });
    }

    let events_file = quote! {
        // Generated CDP event enum. Do not edit by hand.
        // (No inner attributes here — this file is `include!`d.)

        /// Tagged-content enum spanning every event declared by the protocol.
        ///
        /// Wire shape: `{ "method": "Page.frameAttached", "params": { ... } }`.
        #[derive(Debug, Clone, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum CdpEvent {
            #(#variants)*
        }

        impl CdpEvent {
            /// Return the wire `method` name for this event (e.g. `"Page.frameAttached"`).
            pub fn method(&self) -> &'static str {
                match self {
                    #(#method_arms)*
                }
            }
        }
    };
    let events_pretty = prettyplease::unparse(
        &syn::parse2::<syn::File>(events_file).context("parse generated events.rs")?,
    );

    let out_dir = std::env::var_os("OUT_DIR").ok_or_else(|| anyhow::anyhow!("OUT_DIR not set"))?;
    let gen_dir = PathBuf::from(out_dir).join("generated");
    std::fs::create_dir_all(&gen_dir)?;
    std::fs::write(gen_dir.join("domains.rs"), domains_pretty)?;
    std::fs::write(gen_dir.join("events.rs"), events_pretty)?;

    Ok(())
}
