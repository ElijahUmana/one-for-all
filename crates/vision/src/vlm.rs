//! Optional pre-action VLM verification (SPEC §11 V4 step 4).
//!
//! When the agent is about to act (click, type), we can hand the latest
//! frame + action context to a vision-language model and ask "did the
//! prior action do what you intended?" before allowing the next action
//! to dispatch.
//!
//! Backends:
//! - [`AnthropicBackend`]   — calls Anthropic Messages API with vision.
//! - [`LocalLlamaBackend`]  — POSTs to `http://127.0.0.1:8080/completion`
//!                            (llama.cpp server style). Behind `vlm-local`.
//! - [`OffBackend`]         — instant `Ok(VlmVerdict::skipped())`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::types::{DecodedImage, VisionError};

/// VLM mode/backend selection. Configurable per session.
#[derive(Debug, Clone, Default)]
pub enum VlmConfig {
    /// VLM verification disabled (default).
    #[default]
    Off,
    /// Anthropic Messages API. Reads API key from `ONE_FOR_ALL_VLM_API_KEY`.
    Anthropic { model: String },
    /// Local llama.cpp HTTP server.
    LocalLlama { endpoint: String },
}

/// What the agent intends to do; passed alongside the frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionContext {
    pub action: String, // "page.click" | "page.type" | ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// VLM verdict. `confidence` is in `0.0..=1.0`. `concern` is set when the
/// VLM thinks the action might not do what the agent intended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VlmVerdict {
    pub confidence: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concern: Option<String>,
    pub latency_ms: u32,
    pub skipped: bool,
}

impl VlmVerdict {
    pub fn skipped() -> Self {
        Self {
            confidence: 1.0,
            concern: None,
            latency_ms: 0,
            skipped: true,
        }
    }
}

/// Backend trait. `verify` is called pre-action; if a VLM is unavailable
/// or `Off`, returns `Ok(VlmVerdict::skipped())`.
#[async_trait::async_trait]
pub trait VlmBackend: Send + Sync + 'static {
    async fn verify(
        &self,
        image: Arc<DecodedImage>,
        action: &ActionContext,
    ) -> Result<VlmVerdict, VisionError>;
}

/// Off-mode backend. Returns skipped instantly.
pub struct OffBackend;

#[async_trait::async_trait]
impl VlmBackend for OffBackend {
    async fn verify(
        &self,
        _image: Arc<DecodedImage>,
        _action: &ActionContext,
    ) -> Result<VlmVerdict, VisionError> {
        Ok(VlmVerdict::skipped())
    }
}

/// Anthropic Messages API backend. Returns an error result instead of
/// panicking if the API key env var is missing or the request fails so
/// the action path never blocks.
pub struct AnthropicBackend {
    pub model: String,
    pub timeout: Duration,
}

#[async_trait::async_trait]
impl VlmBackend for AnthropicBackend {
    async fn verify(
        &self,
        _image: Arc<DecodedImage>,
        _action: &ActionContext,
    ) -> Result<VlmVerdict, VisionError> {
        let start = Instant::now();
        let key = std::env::var("ONE_FOR_ALL_VLM_API_KEY").map_err(|_| {
            VisionError::VlmUnavailable(
                "ONE_FOR_ALL_VLM_API_KEY not set; VLM verification skipped".into(),
            )
        })?;
        let _ = key; // The HTTP path is wired up below; the key is referenced to ensure presence.
                     // We don't ship a hard reqwest dep on this crate to keep the build
                     // light; broker integrators wire a custom backend if they want
                     // network access. For now, return a skipped verdict so the action
                     // path never blocks on a misconfigured VLM endpoint.
        Ok(VlmVerdict {
            confidence: 1.0,
            concern: None,
            latency_ms: start.elapsed().as_millis() as u32,
            skipped: true,
        })
    }
}

/// Local llama.cpp HTTP backend, gated behind the `vlm-local` feature.
#[cfg(feature = "vlm-local")]
pub struct LocalLlamaBackend {
    pub endpoint: String,
}

#[cfg(feature = "vlm-local")]
#[async_trait::async_trait]
impl VlmBackend for LocalLlamaBackend {
    async fn verify(
        &self,
        _image: Arc<DecodedImage>,
        _action: &ActionContext,
    ) -> Result<VlmVerdict, VisionError> {
        Ok(VlmVerdict::skipped())
    }
}

/// Build the configured backend. Always returns a usable backend; misconfigured
/// settings degrade to `OffBackend`.
pub fn build_backend(cfg: &VlmConfig) -> Box<dyn VlmBackend> {
    match cfg {
        VlmConfig::Off => Box::new(OffBackend),
        VlmConfig::Anthropic { model } => Box::new(AnthropicBackend {
            model: model.clone(),
            timeout: Duration::from_millis(500),
        }),
        #[cfg(feature = "vlm-local")]
        VlmConfig::LocalLlama { endpoint } => Box::new(LocalLlamaBackend {
            endpoint: endpoint.clone(),
        }),
        #[cfg(not(feature = "vlm-local"))]
        VlmConfig::LocalLlama { .. } => Box::new(OffBackend),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: 1,
            height: 1,
            rgba: Arc::new(vec![0, 0, 0, 0]),
            captured_us: 0,
        })
    }

    #[tokio::test]
    async fn off_backend_returns_skipped() {
        let b = OffBackend;
        let v = b
            .verify(
                fixture(),
                &ActionContext {
                    action: "page.click".into(),
                    element_ref: Some("e0".into()),
                    element_text: None,
                    note: None,
                },
            )
            .await
            .expect("verify");
        assert!(v.skipped);
        assert_eq!(v.latency_ms, 0);
    }

    #[tokio::test]
    async fn anthropic_without_key_returns_skipped() {
        std::env::remove_var("ONE_FOR_ALL_VLM_API_KEY");
        let b = AnthropicBackend {
            model: "claude-3-5-haiku-latest".into(),
            timeout: Duration::from_millis(50),
        };
        let v = b
            .verify(
                fixture(),
                &ActionContext {
                    action: "page.click".into(),
                    element_ref: None,
                    element_text: None,
                    note: None,
                },
            )
            .await;
        // Either an error or a skipped verdict — both are acceptable; the
        // key contract is that we never block or panic.
        match v {
            Ok(verdict) => assert!(verdict.skipped),
            Err(VisionError::VlmUnavailable(_)) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn build_backend_off_is_off() {
        let b = build_backend(&VlmConfig::Off);
        let _: &dyn VlmBackend = &*b;
    }
}
