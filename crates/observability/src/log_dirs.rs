//! Resolve and create log directories under `~/.one-for-all/logs/`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Returns the per-component log directory, creating it (mode 0700 on unix) if
/// missing. The directory is `~/.one-for-all/logs/<component>/`.
///
/// `component` must be a simple identifier (alphanumerics, `-`, `_`); anything
/// else is rejected to avoid path traversal.
pub fn for_component(component: &str) -> Result<PathBuf> {
    if component.is_empty()
        || !component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!("invalid component name: {component:?}"));
    }
    let base = base_log_dir()?;
    let dir = base.join(component);
    create_dir_secure(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `~/.one-for-all/logs/`.
pub fn base_log_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = home.join(".one-for-all").join("logs");
    create_dir_secure(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

#[cfg(unix)]
fn create_dir_secure(p: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !p.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_dir_secure(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        assert!(for_component("../etc").is_err());
        assert!(for_component("a/b").is_err());
        assert!(for_component("").is_err());
    }
}
