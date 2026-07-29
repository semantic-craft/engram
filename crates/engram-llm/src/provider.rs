//! [`LlmProvider`] trait — the only abstraction the rest of the
//! workspace depends on.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::error::LlmResult;
use crate::types::{ChatRequest, ChatResponse};

/// Provider-agnostic chat-completion + structured-output API.
///
/// Implementations must be `Send + Sync` so the MCP server / hook
/// router can stash an `Arc<dyn LlmProvider>` and call it from any
/// tokio task. Errors map onto [`LlmError`](crate::LlmError).
///
/// To keep the trait dyn-compatible, the structured-output API takes
/// a raw `serde_json::Value` schema in and yields a raw value out.
/// Use the [`complete_structured`] free function for typed payloads.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Friendly provider name (e.g. `"anthropic"`).
    fn name(&self) -> &'static str;

    /// The model identifier this provider will hit.
    fn model(&self) -> &str;

    /// Plain text completion.
    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse>;

    /// JSON-schema-constrained completion. Returns the unparsed
    /// JSON the provider produced.
    ///
    /// Implementations *must* reject responses with unknown fields at
    /// deserialisation time (cognee #2840 lesson: silent kwarg-drop in
    /// the wrapper is the #1 source of provider-drift bugs).
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value>;

    /// JSON-schema-constrained completion that may issue at most one
    /// upstream model request. Providers whose normal structured path can
    /// retry or fall back must override this method to suppress that retry.
    async fn complete_structured_raw_once(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.complete_structured_raw(request, schema).await
    }
}

/// Typed wrapper around [`LlmProvider::complete_structured_raw`].
///
/// Derives the JSON schema from `T` via `schemars`, calls the provider,
/// then deserialises the response into `T`.
///
/// # Errors
/// Propagates any HTTP, schema, or deserialisation error.
pub async fn complete_structured<T>(
    provider: &(dyn LlmProvider + 'static),
    request: ChatRequest,
) -> LlmResult<T>
where
    T: DeserializeOwned + JsonSchema + Send + 'static,
{
    let schema = serde_json::to_value(schemars::schema_for!(T))?;
    let value = provider.complete_structured_raw(request, schema).await?;
    serde_json::from_value::<T>(value).map_err(crate::LlmError::from)
}

/// Typed wrapper around [`LlmProvider::complete_structured_raw_once`].
///
/// # Errors
/// Propagates any HTTP, schema, or deserialisation error without permitting a
/// provider-level fallback request.
pub async fn complete_structured_once<T>(
    provider: &(dyn LlmProvider + 'static),
    request: ChatRequest,
) -> LlmResult<T>
where
    T: DeserializeOwned + JsonSchema + Send + 'static,
{
    let schema = serde_json::to_value(schemars::schema_for!(T))?;
    let value = provider
        .complete_structured_raw_once(request, schema)
        .await?;
    serde_json::from_value::<T>(value).map_err(crate::LlmError::from)
}
