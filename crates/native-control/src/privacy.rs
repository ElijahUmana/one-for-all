//! SPEC §12 U13 — privacy invariants.
//!
//! Two cross-cut surfaces:
//!
//! 1. **`app_blocklist`** — per-session list of bundle ids the agent may not
//!    touch via any `app.*` call. Substring-any match against the full bundle
//!    id (no globbing — semantics stay tight).
//!
//! 2. **`redact_patterns`** — regex patterns that, when matched against text
//!    we're about to surface to the agent (clipboard contents, AppElement
//!    values), redact the entry. Compiled once on policy installation; we
//!    cache the compiled `Vec<Regex>` and avoid recompiling on the hot path.
//!
//! Implementation discipline:
//! - All redaction happens at READ time, never at WRITE time. The clipboard
//!   ring stores the raw entry; the agent-facing read filters it. Reason: a
//!   policy update must apply to already-captured history.
//! - A bad regex is logged once and ignored — never panics, never poisons
//!   the policy. The unaffected patterns still run.
//! - `app_blocklist` checks happen in the broker's `require_native_for_bundle`
//!   gate, BEFORE any AX FFI runs. Errors are
//!   [`crate::types::NativeControlError::Blocked`].

use parking_lot::RwLock;
use regex::Regex;
use std::sync::Arc;
use tracing::warn;

use crate::types::{ClipboardItem, ClipboardKind, PrivacyPolicy};

/// Compiled redaction engine. Cheap to clone (Arc'd internals).
#[derive(Clone, Default)]
pub struct RedactionEngine {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// Compiled patterns. Order matches input order.
    patterns: Vec<Regex>,
    /// Source patterns kept for debug / introspection.
    sources: Vec<String>,
    /// Bundle id substrings — any one is sufficient to block.
    app_blocklist: Vec<String>,
}

impl std::fmt::Debug for RedactionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read();
        f.debug_struct("RedactionEngine")
            .field("patterns", &inner.sources.len())
            .field("blocklist", &inner.app_blocklist.len())
            .finish()
    }
}

impl RedactionEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a fresh policy. Replaces any previous compiled state.
    /// Bad regexes are logged + skipped; the engine is never poisoned.
    pub fn install(&self, policy: &PrivacyPolicy) {
        let mut compiled: Vec<Regex> = Vec::with_capacity(policy.redact_patterns.len());
        let mut sources: Vec<String> = Vec::with_capacity(policy.redact_patterns.len());
        for src in &policy.redact_patterns {
            match Regex::new(src) {
                Ok(r) => {
                    compiled.push(r);
                    sources.push(src.clone());
                }
                Err(e) => {
                    warn!(pattern = %src, err = %e, "redact_patterns: invalid regex skipped");
                }
            }
        }
        let mut w = self.inner.write();
        w.patterns = compiled;
        w.sources = sources;
        w.app_blocklist = policy.app_blocklist.clone();
    }

    /// True if `bundle_id` is blocked by the session's `app_blocklist`.
    pub fn is_blocked(&self, bundle_id: &str) -> bool {
        let r = self.inner.read();
        if r.app_blocklist.is_empty() {
            return false;
        }
        // Substring match (any list entry being a substring of bundle_id
        // blocks). Empty entries are ignored to prevent a wildcard footgun.
        r.app_blocklist
            .iter()
            .any(|needle| !needle.is_empty() && bundle_id.contains(needle.as_str()))
    }

    /// True if the installed policy contains any non-empty blocklist entry.
    pub fn has_blocklist(&self) -> bool {
        let r = self.inner.read();
        r.app_blocklist.iter().any(|needle| !needle.is_empty())
    }

    /// True if any installed regex matches `s`.
    pub fn matches(&self, s: &str) -> bool {
        let r = self.inner.read();
        r.patterns.iter().any(|re| re.is_match(s))
    }

    /// Apply redaction to a clipboard entry. If any regex hits the inline
    /// text, returns a copy with `text = None`, `redacted = true`. Otherwise
    /// returns the original.
    pub fn apply_clipboard(&self, item: &ClipboardItem) -> ClipboardItem {
        let needs = match (&item.kind, &item.text) {
            (ClipboardKind::String, Some(t)) => self.matches(t),
            _ => false,
        };
        if !needs {
            return item.clone();
        }
        let mut redacted = item.clone();
        redacted.text = None;
        redacted.redacted = true;
        redacted
    }

    /// Apply substring-preserving redaction to free text. Unlike
    /// [`redact_text`](Self::redact_text), this keeps surrounding context and
    /// replaces only the matched spans with `<REDACTED>`. Returns `None` when
    /// no patterns matched.
    pub fn scrub_text(&self, s: &str) -> Option<String> {
        let r = self.inner.read();
        if r.patterns.is_empty() {
            return None;
        }
        let mut matched = false;
        let mut current = s.to_owned();
        for re in &r.patterns {
            if re.is_match(&current) {
                matched = true;
                current = re.replace_all(&current, "<REDACTED>").into_owned();
            }
        }
        matched.then_some(current)
    }

    /// Apply redaction to free text (used for AppElement values flowing into
    /// snapshots). Returns `Some(replacement)` when the text matched a
    /// pattern; the broker swaps in the replacement and stamps `redacted=true`
    /// at the surface layer.
    pub fn redact_text(&self, s: &str) -> Option<String> {
        if !self.matches(s) {
            return None;
        }
        // Replacement format mirrors clipboard: `[redacted len=N]`.
        Some(format!("[redacted len={}]", s.chars().count()))
    }

    /// True if no policy is installed (fast path the hot path can use).
    pub fn is_empty(&self) -> bool {
        let r = self.inner.read();
        r.patterns.is_empty() && r.app_blocklist.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> ClipboardItem {
        ClipboardItem {
            change_count: 1,
            timestamp_ms: 0,
            types: vec!["public.utf8-plain-text".into()],
            kind: ClipboardKind::String,
            text: Some(text.into()),
            files: vec![],
            redacted: false,
        }
    }

    #[test]
    fn empty_policy_is_pass_through() {
        let eng = RedactionEngine::new();
        assert!(eng.is_empty());
        let i = item("hello");
        let out = eng.apply_clipboard(&i);
        assert_eq!(out.text.as_deref(), Some("hello"));
        assert!(!out.redacted);
        assert!(!eng.is_blocked("com.apple.calculator"));
    }

    #[test]
    fn redacts_matching_text() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec![r"\d{3}-\d{2}-\d{4}".into()],
            app_blocklist: vec![],
        });
        let out = eng.apply_clipboard(&item("ssn 123-45-6789"));
        assert!(out.redacted);
        assert!(out.text.is_none());

        let out2 = eng.apply_clipboard(&item("hello world"));
        assert!(!out2.redacted);
        assert_eq!(out2.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn scrub_text_preserves_surrounding_context() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec![r"secret-\d+".into()],
            app_blocklist: vec![],
        });
        let out = eng.scrub_text("before secret-42 after").expect("scrub");
        assert_eq!(out, "before <REDACTED> after");
    }

    #[test]
    fn skips_invalid_regex_without_panic() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec!["[".into(), r"\d+".into()],
            app_blocklist: vec![],
        });
        // First pattern is invalid; second still compiled.
        assert!(eng.matches("abc 1"));
        assert!(!eng.matches("abc"));
    }

    #[test]
    fn app_blocklist_substring_match() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec![],
            app_blocklist: vec!["onepassword".into(), "keychain".into()],
        });
        assert!(eng.is_blocked("com.agilebits.onepassword7"));
        assert!(eng.is_blocked("com.apple.keychainaccess"));
        assert!(!eng.is_blocked("com.apple.calculator"));
        // Empty entries do NOT match everything.
        let eng2 = RedactionEngine::new();
        eng2.install(&PrivacyPolicy {
            redact_patterns: vec![],
            app_blocklist: vec!["".into()],
        });
        assert!(!eng2.is_blocked("com.apple.calculator"));
    }

    #[test]
    fn reports_presence_of_real_blocklist_entries() {
        let eng = RedactionEngine::new();
        assert!(!eng.has_blocklist());
        eng.install(&PrivacyPolicy {
            redact_patterns: vec![],
            app_blocklist: vec!["".into()],
        });
        assert!(!eng.has_blocklist());
        eng.install(&PrivacyPolicy {
            redact_patterns: vec![],
            app_blocklist: vec!["notes".into()],
        });
        assert!(eng.has_blocklist());
    }

    #[test]
    fn replacement_text_format_is_stable() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec!["secret".into()],
            app_blocklist: vec![],
        });
        let out = eng.redact_text("the secret is 42").unwrap();
        // Format: "[redacted len=N]"
        assert!(out.starts_with("[redacted len="));
        assert!(out.ends_with(']'));
        assert!(eng.redact_text("nothing here").is_none());
    }

    #[test]
    fn install_replaces_previous_policy() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec!["foo".into()],
            app_blocklist: vec![],
        });
        assert!(eng.matches("foo bar"));
        eng.install(&PrivacyPolicy {
            redact_patterns: vec!["bar".into()],
            app_blocklist: vec![],
        });
        assert!(eng.matches("hello bar"));
        // The previous pattern is gone.
        assert!(!eng.matches("hello foo"));
    }

    #[test]
    fn binary_clipboard_kinds_are_never_redacted_via_text_path() {
        let eng = RedactionEngine::new();
        eng.install(&PrivacyPolicy {
            redact_patterns: vec![".*".into()],
            app_blocklist: vec![],
        });
        // Files entry has no text — the `.*` pattern would match if text were
        // checked, but we only redact when kind == String && text.is_some().
        let mut i = item("");
        i.text = None;
        i.kind = ClipboardKind::Files;
        i.files = vec!["/Users/a/secret.txt".into()];
        let out = eng.apply_clipboard(&i);
        assert!(!out.redacted);
    }
}
