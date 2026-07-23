//! OpenCode Zen/Go provider.
//!
//! Thin wrapper around [`OpenAiCompatProvider`] that bakes in the OpenCode
//! Zen API base URL (`https://opencode.ai/zen/go/v1`) and names the provider
//! `"opencode"`. Accepts an `sk-...` API key from `OPENCODE_API_KEY`.

use async_trait::async_trait;
use secrecy::SecretString;

use crate::error::LlmResult;
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse};

/// Public OpenCode Zen/Go OpenAI-compatible base URL.
pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Default model when `ENGRAM_LLM_MODEL` is not set.
pub const OPENCODE_DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// OpenCode Zen/Go LLM provider.
///
/// Routes through `https://opencode.ai/zen/go/v1` using the OpenAI chat
/// completions wire format. Authenticate with the `sk-...` key obtained
/// from <https://opencode.ai/auth>.
pub struct OpenCodeProvider {
    inner: OpenAiCompatProvider,
}

impl OpenCodeProvider {
    /// Construct an OpenCode Zen/Go provider.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let inner = OpenAiCompatProvider::new(OPENCODE_ZEN_BASE_URL, Some(api_key), model.into())?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl LlmProvider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.inner.complete(request).await
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.inner.complete_structured_raw(request, schema).await
    }
}
