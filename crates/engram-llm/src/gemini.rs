//! Google Gemini (Generative Language API) client.
//!
//! Structured-output strategy: ask for `responseMimeType:
//! application/json` plus a `responseSchema` — Gemini's native JSON
//! mode. The schema is a *subset* of OpenAPI 3, so we normalise
//! schemars output (Draft 2020-12) by inlining `$ref`s out of
//! `$defs` / `definitions` and stripping keywords Gemini rejects
//! (`$schema`, `additionalProperties`, `oneOf`, `allOf`, `const`,
//! …). See [`prepare_schema_for_gemini`].

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use crate::types::{ChatRequest, ChatResponse, Role, Usage};

/// Default Gemini API base.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Gemini-backed provider.
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
}

impl GeminiProvider {
    /// Construct a provider given an API key and model id (e.g.
    /// `gemini-2.5-flash`).
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        })
    }

    /// Override the API base URL (mostly for tests against wiremock).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[derive(Debug, Serialize)]
struct GeminiRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystem<'a>>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Debug, Serialize)]
struct GeminiContent<'a> {
    role: &'static str,
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct GeminiSystem<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<&'static str>,
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    response_schema: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget")]
    thinking_budget: i32,
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<GeminiUsage>,
    #[serde(rename = "modelVersion", default)]
    model_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiCandidateContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidateContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponsePart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let body = self.build_request(&request, None);
        let response: GeminiResponse = self.post(&body).await?;
        Ok(self.to_chat_response(response))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let prepared = prepare_schema_for_gemini(schema)?;
        let body = self.build_request(&request, Some(prepared));
        let response: GeminiResponse = self.post(&body).await?;
        let text = first_text(&response).ok_or_else(|| {
            LlmError::UnexpectedShape("gemini response had no candidate text".into())
        })?;
        serde_json::from_str::<serde_json::Value>(&text).map_err(LlmError::from)
    }
}

impl GeminiProvider {
    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        response_schema: Option<serde_json::Value>,
    ) -> GeminiRequest<'a> {
        let contents = request
            .messages
            .iter()
            .map(|m| GeminiContent {
                role: match m.role {
                    Role::User => "user",
                    // Gemini's role for the assistant turn is "model".
                    Role::Assistant => "model",
                },
                parts: vec![GeminiPart { text: &m.content }],
            })
            .collect();
        let system_instruction = request.system.as_deref().map(|s| GeminiSystem {
            parts: vec![GeminiPart { text: s }],
        });
        let response_mime_type = response_schema.as_ref().map(|_| "application/json");
        GeminiRequest {
            contents,
            system_instruction,
            generation_config: GeminiGenerationConfig {
                max_output_tokens: request.max_tokens,
                thinking_config: default_thinking_config_for(&self.model),
                temperature: request.temperature,
                response_mime_type,
                response_schema,
            },
        }
    }

    fn to_chat_response(&self, response: GeminiResponse) -> ChatResponse {
        let text = first_text(&response).unwrap_or_default();
        let model = response.model_version.unwrap_or_else(|| self.model.clone());
        ChatResponse {
            text,
            usage: response.usage_metadata.map(|u| Usage {
                input_tokens: u.prompt_token_count,
                output_tokens: u.candidates_token_count,
            }),
            model,
        }
    }

    async fn post<B: Serialize, R: DeserializeOwned>(&self, body: &B) -> LlmResult<R> {
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );
        debug!(url, "POST gemini");
        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", self.api_key.expose_secret())
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        response_json_limited::<R>(resp).await
    }
}

fn default_thinking_config_for(model: &str) -> Option<GeminiThinkingConfig> {
    let model = model.to_ascii_lowercase();
    if model.contains("gemini-2.5-flash") {
        return Some(GeminiThinkingConfig { thinking_budget: 0 });
    }
    None
}

fn first_text(response: &GeminiResponse) -> Option<String> {
    let candidate = response.candidates.first()?;
    let content = candidate.content.as_ref()?;
    let joined: String = content
        .parts
        .iter()
        .filter_map(|p| p.text.as_deref())
        .collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Normalise a schemars-generated JSON Schema into the Gemini-supported
/// OpenAPI 3 subset.
///
/// Gemini's `responseSchema` rejects Draft-2020-12 specifics: `$schema`,
/// `$defs` / `definitions`, `$ref`, `additionalProperties`, `oneOf`,
/// `allOf`, `const`, and assorted metadata. We inline all `$ref`s out
/// of `$defs` / `definitions`, then recursively strip the keywords
/// Gemini won't accept. `anyOf` is preserved (the only composition
/// keyword Gemini accepts).
///
/// Finally we normalise `type` *arrays* (schemars emits `["string",
/// "null"]` for `Option<T>`) into a single `type` plus `nullable: true`,
/// which is the shape Gemini's OpenAPI-3 subset expects — see
/// [`normalize_nullable_types`].
///
/// # Errors
/// Returns [`LlmError::Schema`] if a `$ref` can't be resolved or the
/// schema's reference graph exceeds a safety depth (16).
pub fn prepare_schema_for_gemini(mut schema: serde_json::Value) -> LlmResult<serde_json::Value> {
    let defs = extract_defs(&mut schema);
    inline_refs(&mut schema, &defs, 0)?;
    strip_unsupported(&mut schema);
    normalize_nullable_types(&mut schema);
    Ok(schema)
}

fn extract_defs(schema: &mut serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut defs = serde_json::Map::new();
    let Some(obj) = schema.as_object_mut() else {
        return defs;
    };
    if let Some(serde_json::Value::Object(d)) = obj.remove("$defs") {
        for (k, v) in d {
            defs.insert(k, v);
        }
    }
    if let Some(serde_json::Value::Object(d)) = obj.remove("definitions") {
        for (k, v) in d {
            defs.insert(k, v);
        }
    }
    defs
}

const MAX_REF_DEPTH: usize = 16;

fn inline_refs(
    value: &mut serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    depth: usize,
) -> LlmResult<()> {
    if depth > MAX_REF_DEPTH {
        return Err(LlmError::Schema(
            "recursive $ref depth exceeded while preparing gemini schema".into(),
        ));
    }
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(reference)) = map.get("$ref").cloned() {
                let name = reference.rsplit('/').next().unwrap_or_default();
                let mut resolved = defs.get(name).cloned().ok_or_else(|| {
                    LlmError::Schema(format!("gemini: unresolved $ref {reference}"))
                })?;
                inline_refs(&mut resolved, defs, depth + 1)?;
                *value = resolved;
                return Ok(());
            }
            for v in map.values_mut() {
                inline_refs(v, defs, depth + 1)?;
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                inline_refs(v, defs, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

const UNSUPPORTED_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "$defs",
    "definitions",
    "additionalProperties",
    "unevaluatedProperties",
    "allOf",
    "oneOf",
    "const",
    "default",
    "examples",
    "patternProperties",
    "readOnly",
    "writeOnly",
];

fn strip_unsupported(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for k in UNSUPPORTED_KEYS {
                map.remove(*k);
            }
            for v in map.values_mut() {
                strip_unsupported(v);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                strip_unsupported(v);
            }
        }
        _ => {}
    }
}

/// Rewrite JSON-Schema `type` *arrays* into the single-value form Gemini
/// requires.
///
/// schemars encodes `Option<T>` as `type: ["<t>", "null"]` (Draft 2020-12).
/// Gemini's `responseSchema` only accepts a single `type` string, so it
/// rejects the array with `400 INVALID_ARGUMENT … "type" … Proto field is
/// not repeating, cannot start list`. We collapse each such array:
///
/// - `["<t>", "null"]` → `type: "<t>"`, `nullable: true`
/// - `["null"]` (or empty) → drop `type`, keep `nullable: true`
/// - a genuine multi-type union → `anyOf` of single-`type` schemas
///   (plus `nullable: true` when `"null"` was present)
///
/// `anyOf` is the only composition keyword Gemini accepts, so the union
/// fallback stays within the supported subset.
fn normalize_nullable_types(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(variants)) = map.get("type").cloned() {
                let mut non_null: Vec<String> = Vec::new();
                let mut nullable = false;
                for variant in &variants {
                    match variant.as_str() {
                        Some("null") => nullable = true,
                        Some(other) => non_null.push(other.to_string()),
                        None => {}
                    }
                }
                match non_null.as_slice() {
                    [single] => {
                        map.insert("type".into(), serde_json::Value::String(single.clone()));
                    }
                    [] => {
                        map.remove("type");
                    }
                    _ => {
                        map.remove("type");
                        let any_of = non_null
                            .iter()
                            .map(|t| serde_json::json!({ "type": t }))
                            .collect();
                        map.insert("anyOf".into(), serde_json::Value::Array(any_of));
                    }
                }
                if nullable {
                    map.insert("nullable".into(), serde_json::Value::Bool(true));
                }
            }
            for v in map.values_mut() {
                normalize_nullable_types(v);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                normalize_nullable_types(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prepare_schema_inlines_defs_and_strips_metadata() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "ConsolidatedBatch",
            "type": "object",
            "properties": {
                "updates": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/Update" }
                }
            },
            "required": ["updates"],
            "additionalProperties": false,
            "$defs": {
                "Update": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        });
        let prepared = prepare_schema_for_gemini(schema).unwrap();
        let obj = prepared.as_object().unwrap();
        assert!(!obj.contains_key("$schema"));
        assert!(!obj.contains_key("$defs"));
        assert!(!obj.contains_key("additionalProperties"));
        let item = obj
            .get("properties")
            .unwrap()
            .get("updates")
            .unwrap()
            .get("items")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(item.get("type").unwrap(), "object");
        assert!(!item.contains_key("additionalProperties"));
        assert_eq!(
            item.get("properties").unwrap().get("path").unwrap(),
            &json!({"type": "string"})
        );
    }

    #[test]
    fn prepare_schema_rejects_unresolved_ref() {
        let schema = json!({ "$ref": "#/$defs/Missing" });
        let err = prepare_schema_for_gemini(schema).unwrap_err();
        assert!(matches!(err, LlmError::Schema(_)));
    }

    #[test]
    fn prepare_schema_passes_plain_object_through() {
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let prepared = prepare_schema_for_gemini(schema.clone()).unwrap();
        assert_eq!(prepared, schema);
    }

    #[test]
    fn prepare_schema_preserves_any_of() {
        let schema = json!({
            "anyOf": [
                { "type": "string" },
                { "type": "integer" }
            ]
        });
        let prepared = prepare_schema_for_gemini(schema.clone()).unwrap();
        assert_eq!(prepared, schema);
    }

    #[test]
    fn prepare_schema_collapses_nullable_type_array() {
        // schemars emits `["string", "null"]` for `Option<String>`; Gemini
        // rejects the array and wants `type: "string", nullable: true`.
        let schema = json!({
            "type": "object",
            "properties": {
                "title": { "type": ["string", "null"] },
                "count": { "type": "integer" }
            },
            "required": ["title"]
        });
        let prepared = prepare_schema_for_gemini(schema).unwrap();
        let title = prepared.pointer("/properties/title").unwrap();
        assert_eq!(title.get("type").unwrap(), &json!("string"));
        assert_eq!(title.get("nullable").unwrap(), &json!(true));
        // non-nullable siblings are untouched.
        let count = prepared.pointer("/properties/count").unwrap();
        assert_eq!(count, &json!({ "type": "integer" }));
    }

    #[test]
    fn prepare_schema_collapses_nested_nullable_in_array_items() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": { "note": { "type": ["string", "null"] } }
            }
        });
        let prepared = prepare_schema_for_gemini(schema).unwrap();
        let note = prepared.pointer("/items/properties/note").unwrap();
        assert_eq!(note.get("type").unwrap(), &json!("string"));
        assert_eq!(note.get("nullable").unwrap(), &json!(true));
    }

    #[test]
    fn prepare_schema_multi_type_union_becomes_any_of() {
        let schema = json!({ "type": ["string", "integer", "null"] });
        let prepared = prepare_schema_for_gemini(schema).unwrap();
        assert!(prepared.get("type").is_none());
        assert_eq!(prepared.get("nullable").unwrap(), &json!(true));
        let any_of = prepared.get("anyOf").unwrap().as_array().unwrap();
        assert_eq!(
            any_of,
            &vec![json!({ "type": "string" }), json!({ "type": "integer" })]
        );
    }

    #[test]
    fn prepare_schema_null_only_type_array_drops_type_keeps_nullable() {
        let schema = json!({ "type": ["null"] });
        let prepared = prepare_schema_for_gemini(schema).unwrap();
        assert!(prepared.get("type").is_none());
        assert!(prepared.get("anyOf").is_none());
        assert_eq!(prepared.get("nullable").unwrap(), &json!(true));
    }

    #[test]
    fn build_request_disables_default_thinking_for_25_flash() {
        let provider =
            GeminiProvider::new(SecretString::from("test-key"), "gemini-2.5-flash").unwrap();
        let request = ChatRequest::user_prompt("emit json");
        let body = serde_json::to_value(provider.build_request(&request, None)).unwrap();
        assert_eq!(
            body.pointer("/generationConfig/thinkingConfig/thinkingBudget"),
            Some(&json!(0))
        );
    }

    #[test]
    fn build_request_omits_thinking_config_for_non_flash_models() {
        let provider =
            GeminiProvider::new(SecretString::from("test-key"), "gemini-2.5-pro").unwrap();
        let request = ChatRequest::user_prompt("emit json");
        let body = serde_json::to_value(provider.build_request(&request, None)).unwrap();
        assert!(body.pointer("/generationConfig/thinkingConfig").is_none());
    }
}
