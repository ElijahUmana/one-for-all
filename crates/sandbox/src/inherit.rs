//! `inherit` parameter: which slices of the user's host state get cloned
//! into a new session's sandbox rootfs, and at what permission level inside
//! the sandbox-exec profile.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::errors::Error;
use crate::Result;

/// Read/write semantics inside the sandbox profile. The clonefile itself
/// always produces a writable on-disk copy — `ReadOnly` here means the
/// sandbox-exec profile denies the agent any write subpolicy on this tree.
/// That gives us "user could not corrupt their host SSH config even by
/// writing to the cloned copy" without paying for a second copy on
/// promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InheritMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InheritSpec {
    /// Source on the host filesystem. Must be absolute.
    pub host_path: PathBuf,
    /// Sandbox permission inside the cloned copy.
    pub mode: InheritMode,
}

impl InheritSpec {
    pub fn rw(host_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            mode: InheritMode::ReadWrite,
        }
    }

    pub fn ro(host_path: impl Into<PathBuf>) -> Self {
        Self {
            host_path: host_path.into(),
            mode: InheritMode::ReadOnly,
        }
    }
}

/// SPEC-default safe set, applied when the caller doesn't pass `inherit`.
///
/// **Conservative by design** (per V3.1 hotfix): real-host
/// `clonefile(2)` of `~/Documents` hits `EDEADLK` on machines with
/// active Spotlight / Time Machine / iCloud sync workers, and many
/// hosts have `~/Documents` at 50GB+ which blows the 3s register SLO.
/// The default is now ONLY `~/Downloads` (small, agent-friendly target
/// for downloads); `~/Documents`, dotfiles, and SSH keys must be
/// requested explicitly via the `inherit` array on `session.register`.
///
/// Explicit opt-in keys (parsed by `parse_inherit_keys`):
///   `documents`, `ssh-readonly`, `config-readonly`, `<absolute path>`.
pub fn default_allowlist() -> Vec<InheritSpec> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    vec![InheritSpec::rw(home.join("Downloads"))]
}

/// Parse the wire-level `inherit: ["cookies", "downloads", ...]` array sent
/// on `session.register` / `browser.context.create`.
///
/// Recognised keys:
/// - `"cookies"` — implicit (Chrome profile is cloned regardless), but
///   accepted as a no-op so clients that explicitly opt in don't error.
/// - `"downloads"` → RW `~/Downloads`.
/// - `"documents"` → RW `~/Documents`.
/// - `"ssh-readonly"` → RO `~/.ssh`.
/// - `"config-readonly"` → RO `~/.config`.
/// - Any string starting with `/` → absolute path, RW.
///
/// Unknown keys yield `Error::InvalidInheritKey`.
pub fn parse_inherit_keys(keys: &[String]) -> Result<Vec<InheritSpec>> {
    let home = dirs::home_dir().ok_or(Error::HomeDirUnresolvable)?;
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        match k.as_str() {
            "cookies" => {
                // No filesystem inherit — the Chrome profile clone covers
                // this. Accept the key for clarity in client code.
            }
            "downloads" => out.push(InheritSpec::rw(home.join("Downloads"))),
            "documents" => out.push(InheritSpec::rw(home.join("Documents"))),
            "ssh-readonly" => out.push(InheritSpec::ro(home.join(".ssh"))),
            "config-readonly" => out.push(InheritSpec::ro(home.join(".config"))),
            other if other.starts_with('/') => {
                out.push(InheritSpec::rw(PathBuf::from(other)));
            }
            other => return Err(Error::InvalidInheritKey(other.to_string())),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_output() {
        let r = parse_inherit_keys(&[]).expect("ok");
        assert!(r.is_empty());
    }

    #[test]
    fn unknown_key_errors() {
        let err = parse_inherit_keys(&["definitely-not-a-key".into()]).unwrap_err();
        assert!(matches!(err, Error::InvalidInheritKey(_)));
    }

    #[test]
    fn known_keys_resolve_to_specs() {
        let r = parse_inherit_keys(&["downloads".into(), "ssh-readonly".into(), "cookies".into()])
            .expect("ok");
        // cookies emits no spec; downloads + ssh do.
        assert_eq!(r.len(), 2);
        assert!(matches!(r[0].mode, InheritMode::ReadWrite));
        assert!(matches!(r[1].mode, InheritMode::ReadOnly));
        assert!(r[1].host_path.ends_with(".ssh"));
    }

    #[test]
    fn absolute_path_is_rw() {
        let r = parse_inherit_keys(&["/tmp/agent-root".into()]).expect("ok");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].host_path, PathBuf::from("/tmp/agent-root"));
        assert_eq!(r[0].mode, InheritMode::ReadWrite);
    }

    #[test]
    fn default_allowlist_is_minimal_safe_set() {
        // Hotfix: default is Downloads-only (Documents was hitting
        // EDEADLK on real hosts). Documents/dotfiles require explicit
        // opt-in via `inherit`.
        if dirs::home_dir().is_none() {
            return;
        }
        let a = default_allowlist();
        assert_eq!(a.len(), 1, "default allowlist must be minimal");
        assert!(a[0].host_path.ends_with("Downloads"));
        assert_eq!(a[0].mode, InheritMode::ReadWrite);
    }
}
