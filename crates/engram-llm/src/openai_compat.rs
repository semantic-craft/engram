//! OpenAI-compatible client (Ollama, vLLM, LM Studio, llama.cpp).
//!
//! Uses the same wire format as [`crate::openai::OpenAiProvider`] but
//! with a configurable base URL (and no key required for most local
//! deployments). Structured output defaults to "parse first JSON object
//! out of the text" because many local engines lack reliable
//! `response_format` honour; operators can opt into strict
//! `response_format=json_schema` for engines that support it.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use secrecy::SecretString;
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai::{OpenAiProvider, RequestDialect, enforce_strict_object_schemas};
use crate::provider::LlmProvider;
use crate::text::{suffix_within_bytes, truncate_with_ellipsis};
use crate::types::{ChatRequest, ChatResponse};

// Compiled once. Matches <think>, <thinking>, <analysis>, <reasoning> blocks
// (case-insensitive, non-greedy, DOTALL) that reasoning models emit before
// the JSON payload. These blocks often contain `{` braces that confuse the
// balanced-brace extractor when unstripped.
//
// Each tag is listed explicitly because the `regex` crate does not support
// backreferences — we cannot write `<(tag)>.*?</\1>`.
static REASONING_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)(?:<think>.*?</think>|<thinking>.*?</thinking>|<analysis>.*?</analysis>|<reasoning>.*?</reasoning>)",
    )
    .unwrap()
});

// Matches an outermost ```json ... ``` or ``` ... ``` markdown fence.
static CODE_FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\A\s*```(?:json)?\s*\n(.*?)\n\s*```\s*\z").unwrap());

/// Remove `<think>`, `<thinking>`, `<analysis>`, and `<reasoning>` blocks
/// (case-insensitive) from `s`, then unwrap any surrounding markdown code
/// fence. Returns a freshly allocated `String` so the caller can choose
/// between the cleaned and the raw form.
pub(crate) fn strip_reasoning_blocks(s: &str) -> String {
    let stripped = REASONING_BLOCK_RE.replace_all(s, "");
    // If the entire remaining payload is wrapped in a code fence, unwrap it
    // so `serde_json::from_str` can see the bare JSON directly.
    if let Some(caps) = CODE_FENCE_RE.captures(stripped.as_ref()) {
        caps[1].trim().to_string()
    } else {
        stripped.trim().to_string()
    }
}

/// OpenAI-compatible provider, parameterised by base URL.
pub struct OpenAiCompatProvider {
    inner: OpenAiProvider,
    name_tag: &'static str,
    /// When `true`, structured output sends a strict `response_format`
    /// instead of the tolerant parser. Set by the factory from
    /// `ProviderConfig::compat_strict` (sourced once by `Config::load`).
    strict: bool,
}

impl OpenAiCompatProvider {
    /// Construct a provider pointed at `base_url` (`LLM_BASE_URL` or
    /// `OLLAMA_HOST`). API key is optional; many local engines accept
    /// any non-empty string.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<SecretString>,
        model: impl Into<String>,
    ) -> LlmResult<Self> {
        let key = api_key.unwrap_or_else(|| SecretString::from("dummy"));
        // Local / proxy engines speak the legacy OpenAI wire format
        // only — no `max_completion_tokens`, no model-family caps, no
        // temperature massaging. Swap dialect so the inner provider's
        // per-request quirks don't leak into Ollama / vLLM setups.
        let inner = OpenAiProvider::new(key, model)?
            .with_base_url(base_url)
            .with_dialect(RequestDialect::Compat);
        Ok(Self {
            inner,
            name_tag: "openai-compat",
            strict: false,
        })
    }

    /// Set strict mode. The factory calls this with
    /// `ProviderConfig::compat_strict`; `new` defaults it off so the
    /// tolerant parser stays the zero-config behaviour.
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &'static str {
        self.name_tag
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
        // Strict mode (opt-in; see `strict` field): modern local engines
        // honour `response_format=json_schema`. Normalise the schema
        // to the strict subset (additionalProperties:false, full required, no
        // oneOf / no $ref siblings) — the same rewrite the Official dialect
        // applies — and delegate to the inner provider, which sends the
        // response_format. The dialect stays Compat, so the api.openai.com
        // token/temperature quirks do NOT leak into the local engine.
        //
        // Note: `enforce_strict_object_schemas` is invoked here because the
        // inner provider's own normalisation is gated on `RequestDialect::
        // Official` (see openai.rs::complete_structured_raw). The wrapper
        // owns the Compat-side rewrite so a refactor of the inner cannot
        // silently drop it.
        //
        // Fallback path:
        // Fallback only on *parse-shape* failures — the engine returned a
        // response but it wasn't a valid JSON object (the case the tolerant
        // path was designed for). On `LlmError::Provider` (HTTP 4xx/5xx,
        // auth, rate-limit) and transport-level failures, propagate the
        // error: a `complete()` retry inside the fallback would hit the
        // same wall and double the latency / token spend for no gain.
        //
        // The fallback itself is a SECOND HTTP call to the model — the
        // strict raw call returns parsed JSON or an error, not raw text,
        // so we can't reuse the response body. Operators on engines that
        // routinely fall back (e.g. reasoning models with `<think>` in
        // `content`) should keep `strict=false` to avoid the double call.
        if self.strict {
            let mut strict_schema = schema.clone();
            enforce_strict_object_schemas(&mut strict_schema);
            match self
                .inner
                .complete_structured_raw(request.clone(), strict_schema)
                .await
            {
                Ok(v) if v.is_object() => return Ok(v),
                Ok(_) => {
                    debug!("compat strict: non-object response, falling back to tolerant parser");
                }
                Err(err) if is_parse_shape_error(&err) => {
                    debug!(error = %err, "compat strict parse-shape mismatch, falling back to tolerant parser");
                }
                Err(err) => {
                    // Transport / 5xx / auth / rate-limit: a tolerant retry
                    // would hit the same failure. Propagate so the caller
                    // sees the real cause and can back off appropriately.
                    return Err(err);
                }
            }
        }
        // Default (and strict fallback): most older local engines don't
        // honour `response_format`. Ask for JSON and extract the first
        // balanced `{…}` object from the text.
        let res = self.inner.complete(request).await?;
        // Reasoning models (DeepSeek, Qwen, MiniMax M2.7, …) prepend
        // `<think>…</think>` before the JSON. Strip those blocks (and any
        // surrounding markdown fences) before trying to parse — otherwise
        // `first_json_object` latches onto a `{` inside the reasoning text.
        let cleaned = strip_reasoning_blocks(&res.text);
        match serde_json::from_str::<serde_json::Value>(&cleaned) {
            Ok(v) if v.is_object() => Ok(v),
            _ => {
                let Some(slice) = first_json_object(&cleaned) else {
                    // Dump enough text to actually see what the model
                    // returned. 200 chars truncates inside code fences;
                    // 4 KB tells the full story for any reasonable
                    // structured-output response. Includes head + tail
                    // because some failures truncate the closing brace.
                    let head = truncate_with_ellipsis(&cleaned, 2000);
                    let tail = suffix_within_bytes(&cleaned, 2000);
                    debug!(
                        head = %head,
                        tail = %tail,
                        total_len = cleaned.len(),
                        "no balanced JSON object found"
                    );
                    return Err(LlmError::UnexpectedShape(
                        "openai-compat response did not contain a JSON object".into(),
                    ));
                };
                serde_json::from_str::<serde_json::Value>(slice).map_err(LlmError::from)
            }
        }
    }
}

/// True for errors where a *retry without structured-output coercion*
/// has a real chance of succeeding — i.e. the upstream returned a
/// response, but it wasn't in the shape we wanted. Transport / HTTP /
/// auth errors are excluded because they would just reproduce.
fn is_parse_shape_error(err: &LlmError) -> bool {
    matches!(err, LlmError::UnexpectedShape(_) | LlmError::Serde(_))
}

/// Find the first balanced `{...}` object in a string, skipping
/// braces that appear inside JSON string literals.
///
/// The naive implementation (only count `{` / `}`) breaks when the
/// model returns markdown content inside a JSON string value — the
/// content commonly contains `{` and `}` in code examples,
/// JSON-as-prose, etc. That throws the depth counter off and
/// either truncates the object early or never closes it. This
/// version tracks whether we're inside a `"..."` literal and
/// honours backslash escapes the JSON spec defines.
fn first_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    let bytes = s.as_bytes();
    for (i, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new` defaults strict off; `with_strict` is the only way to turn it
    /// on, mirroring how the factory threads `ProviderConfig::compat_strict`.
    #[test]
    fn strict_defaults_off_and_can_be_overridden() {
        let p = OpenAiCompatProvider::new("http://localhost:11434/v1", None, "mistral-nemo")
            .expect("provider builds");
        assert!(!p.strict);
        let p = p.with_strict(true);
        assert!(p.strict);
    }

    /// The strict path's fallback policy lives in `is_parse_shape_error`:
    /// fall back ONLY when the upstream returned a response in the wrong
    /// shape (the tolerant prose-JSON parser has a real chance). Propagate
    /// transport / HTTP-status / auth errors — a tolerant retry would just
    /// hit the same wall and double cost. Regression guard for the audit
    /// finding that the original strict-fallback caught every `Err(_)`.
    #[test]
    fn is_parse_shape_error_classifies_correctly() {
        // Shape failures → fall back.
        assert!(is_parse_shape_error(&LlmError::UnexpectedShape(
            "no JSON object".into()
        )));
        assert!(is_parse_shape_error(&LlmError::Serde(
            "trailing comma".into()
        )));
        // Transport / HTTP / auth → propagate.
        assert!(!is_parse_shape_error(&LlmError::Provider {
            status: 500,
            body: "engine boom".into()
        }));
        assert!(!is_parse_shape_error(&LlmError::Provider {
            status: 401,
            body: "unauthorized".into()
        }));
        assert!(!is_parse_shape_error(&LlmError::Provider {
            status: 429,
            body: "rate limited".into()
        }));
    }

    #[test]
    fn first_json_object_finds_balanced_object() {
        assert_eq!(first_json_object("noise {\"k\":1} more"), Some("{\"k\":1}"));
        assert_eq!(
            first_json_object("text {\"a\":{\"b\":2}} trailing"),
            Some("{\"a\":{\"b\":2}}"),
        );
        assert_eq!(first_json_object("no json here"), None);
    }

    // ── strip_reasoning_blocks ───────────────────────────────────────────

    /// A `<think>` block whose body contains braces must be stripped so the
    /// real JSON object is found rather than a brace inside the think text.
    #[test]
    fn strip_think_block_with_braces_before_json() {
        let input = "<think>maybe {a:1} or {b:2}?</think>\n{\"findings\":[]}";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value =
            serde_json::from_str(&cleaned).expect("should parse to the real JSON object");
        assert!(v.is_object());
        assert_eq!(v["findings"], serde_json::json!([]));
    }

    /// `<analysis>` prefix is also stripped.
    #[test]
    fn strip_analysis_block() {
        let input = "<analysis>I reviewed the pages.</analysis>\n{\"key\":\"value\"}";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value = serde_json::from_str(&cleaned).expect("parse");
        assert_eq!(v["key"], "value");
    }

    /// A lone ```` ```json\n{...}\n``` ```` fence is unwrapped so the JSON
    /// is directly parseable.
    #[test]
    fn strip_json_code_fence() {
        let input = "```json\n{\"answer\":42}\n```";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value = serde_json::from_str(&cleaned).expect("parse");
        assert_eq!(v["answer"], 42);
    }

    /// A plain JSON object with no reasoning prefix must survive unchanged
    /// (modulo trimming).
    #[test]
    fn plain_json_unchanged() {
        let input = "{\"hello\":\"world\"}";
        let cleaned = strip_reasoning_blocks(input);
        assert_eq!(cleaned, input);
    }

    /// Tag matching is case-insensitive — `<THINK>` is stripped just like
    /// `<think>`.
    #[test]
    fn strip_is_case_insensitive() {
        let input = "<THINK>uppercase</THINK>\n{\"ok\":true}";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value = serde_json::from_str(&cleaned).expect("parse");
        assert_eq!(v["ok"], true);
    }
}
