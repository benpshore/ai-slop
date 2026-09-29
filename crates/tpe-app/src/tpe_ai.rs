//! Minimal model-provider client for the Ask panel.
//!
//! Two providers over plain HTTPS with `ureq`:
//!
//! - Anthropic Messages API: `POST https://api.anthropic.com/v1/messages` with
//!   `x-api-key` and `anthropic-version: 2023-06-01`; body
//!   `{model, max_tokens, system, messages:[{role:"user", content}]}`. The reply's
//!   `content` is an array of blocks; the text blocks are concatenated (a
//!   `thinking` block may come first, so `content[0]` is not assumed to be text).
//! - `OpenAI` chat completions: `POST https://api.openai.com/v1/chat/completions`
//!   with `Authorization: Bearer`; body `{model, messages}`; the reply's
//!   `choices[0].message.content` is returned.
//!
//! The request builders and response parsers are pure and unit tested on
//! recorded JSON; only [`ask`] performs I/O, and no test calls it with a key.

use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;

use tpe_credentials::ApiKeys;

/// Anthropic Messages endpoint.
pub const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
/// `OpenAI` chat completions endpoint.
pub const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
/// Google Gemini generate-content endpoint.
pub const GEMINI_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent";
/// Default trusted-local Ollama chat endpoint.
pub const OLLAMA_URL: &str = "http://127.0.0.1:11434/api/chat";
/// Required Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic model id used by the panel.
pub const ANTHROPIC_MODEL: &str = "claude-sonnet-5";
/// `OpenAI` model id used by the panel.
pub const OPENAI_MODEL: &str = "gpt-5";
/// Gemini model id used by the panel.
pub const GEMINI_MODEL: &str = "gemini-2.5-flash";
/// Ollama model id used by the panel.
pub const OLLAMA_MODEL: &str = "llama3.2";
/// Output token ceiling for a non-streaming answer in the panel.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
/// End-to-end timeout for one request.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// `User-Agent` sent with every request.
pub const USER_AGENT: &str = "tpe-app/0.1 (+https://github.com/benpshore/text-processing-engine)";
/// Longest error-body excerpt kept in an error message.
const ERROR_EXCERPT_CHARS: usize = 300;

/// Whether a provider endpoint is cloud-fixed or defaults to a trusted local service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointPolicy {
    Cloud,
    TrustedLocal,
}
/// Authentication expected by a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authentication {
    RequiredBearer,
    RequiredHeader,
    OptionalBearer,
}
impl Authentication {
    pub const fn required(self) -> bool {
        !matches!(self, Self::OptionalBearer)
    }
}
/// All metadata needed to address a model provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderMetadata {
    pub label: &'static str,
    pub credential_service: &'static str,
    pub default_model: &'static str,
    pub endpoint: &'static str,
    pub endpoint_policy: EndpointPolicy,
    pub authentication: Authentication,
}
/// Which model provider answers the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAI,
    Anthropic,
    Gemini,
    Ollama,
}
impl Provider {
    pub const ALL: [Self; 4] = [Self::OpenAI, Self::Anthropic, Self::Gemini, Self::Ollama];
    pub const fn metadata(self) -> &'static ProviderMetadata {
        &PROVIDERS[self as usize]
    }
    pub fn label(self) -> &'static str {
        self.metadata().label
    }
    pub fn endpoint(self) -> &'static str {
        self.metadata().endpoint
    }
    pub fn model(self) -> &'static str {
        self.metadata().default_model
    }
    pub fn credential_service(self) -> &'static str {
        self.metadata().credential_service
    }
    /// Deterministically select the next entry in the provider registry.
    #[must_use]
    pub fn next(self) -> Self {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }
}
/// Canonical provider registry, ordered like [`Provider::ALL`].
pub const PROVIDERS: [ProviderMetadata; 4] = [
    ProviderMetadata {
        label: "ChatGPT",
        credential_service: ApiKeys::OPENAI,
        default_model: OPENAI_MODEL,
        endpoint: OPENAI_URL,
        endpoint_policy: EndpointPolicy::Cloud,
        authentication: Authentication::RequiredBearer,
    },
    ProviderMetadata {
        label: "Claude",
        credential_service: ApiKeys::ANTHROPIC,
        default_model: ANTHROPIC_MODEL,
        endpoint: ANTHROPIC_URL,
        endpoint_policy: EndpointPolicy::Cloud,
        authentication: Authentication::RequiredHeader,
    },
    ProviderMetadata {
        label: "Gemini",
        credential_service: ApiKeys::GEMINI,
        default_model: GEMINI_MODEL,
        endpoint: GEMINI_URL,
        endpoint_policy: EndpointPolicy::Cloud,
        authentication: Authentication::RequiredHeader,
    },
    ProviderMetadata {
        label: "Ollama",
        credential_service: ApiKeys::OLLAMA,
        default_model: OLLAMA_MODEL,
        endpoint: OLLAMA_URL,
        endpoint_policy: EndpointPolicy::TrustedLocal,
        authentication: Authentication::OptionalBearer,
    },
];

/// Errors from [`ask`] and the parsers. Messages never include the API key.
#[derive(Debug, Error)]
pub enum AiError {
    /// No key was supplied.
    #[error("no API key supplied")]
    EmptyKey,
    /// The question was blank.
    #[error("the question is empty")]
    EmptyQuestion,
    /// Connection, TLS, timeout or body-read failure.
    #[error("transport: {0}")]
    Transport(String),
    /// Non-2xx HTTP status; `message` is the provider's error text or a body excerpt.
    #[error("HTTP {code}: {message}")]
    Status {
        /// HTTP status code.
        code: u16,
        /// Provider error message or body excerpt.
        message: String,
    },
    /// The body was not JSON.
    #[error("response is not JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A 2xx body that is a provider error object.
    #[error("API error ({kind}): {message}")]
    Api {
        /// Provider error type.
        kind: String,
        /// Provider error message.
        message: String,
    },
    /// The model declined; the string carries the provider's explanation when given.
    #[error("the model declined to answer{0}")]
    Refused(String),
    /// A 2xx body with no text to show.
    #[error("response contains no text")]
    NoText,
}

impl From<ureq::Error> for AiError {
    fn from(error: ureq::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

/// Builds the JSON request body for `provider`.
pub fn request_body(provider: Provider, system: &str, user: &str, max_tokens: u32) -> Value {
    match provider {
        Provider::Anthropic => json!({
            "model": provider.model(),
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        }),
        Provider::OpenAI => json!({
            "model": provider.model(),
            "messages": [ { "role": "system", "content": system }, { "role": "user", "content": user } ],
        }),
        Provider::Gemini => json!({
            "system_instruction": { "parts": [{ "text": system }] },
            "contents": [{ "role": "user", "parts": [{ "text": user }] }],
            "generationConfig": { "maxOutputTokens": max_tokens },
        }),
        Provider::Ollama => json!({
            "model": provider.model(), "stream": false,
            "messages": [ { "role": "system", "content": system }, { "role": "user", "content": user } ],
        }),
    }
}

/// Request headers for `provider`, including the authentication header.
pub fn headers(provider: Provider, api_key: &str) -> Vec<(&'static str, String)> {
    let mut out = vec![("content-type", "application/json".to_owned())];
    match provider {
        Provider::Anthropic => {
            out.push(("x-api-key", api_key.to_owned()));
            out.push(("anthropic-version", ANTHROPIC_VERSION.to_owned()));
        }
        Provider::OpenAI => {
            out.push(("authorization", format!("Bearer {api_key}")));
        }
        Provider::Gemini => out.push(("x-goog-api-key", api_key.to_owned())),
        Provider::Ollama if !api_key.trim().is_empty() => {
            out.push(("authorization", format!("Bearer {api_key}")));
        }
        Provider::Ollama => {}
    }
    out
}

/// Extracts the answer text from a 2xx response body.
pub fn parse_response(provider: Provider, body: &str) -> Result<String, AiError> {
    let value: Value = serde_json::from_str(body)?;
    if let Some((kind, message)) = error_object(&value) {
        return Err(AiError::Api { kind, message });
    }
    match provider {
        Provider::Anthropic => parse_anthropic(&value),
        Provider::OpenAI => parse_openai(&value),
        Provider::Gemini => parse_gemini(&value),
        Provider::Ollama => parse_ollama(&value),
    }
}

fn parse_anthropic(value: &Value) -> Result<String, AiError> {
    let empty: &[Value] = &[];
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .map_or(empty, Vec::as_slice);
    let text: String = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if !text.is_empty() {
        return Ok(text);
    }
    if value.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        let details = value.get("stop_details");
        let explanation = details
            .and_then(|d| d.get("explanation"))
            .and_then(Value::as_str)
            .or_else(|| {
                details
                    .and_then(|d| d.get("category"))
                    .and_then(Value::as_str)
            })
            .map_or_else(String::new, |why| format!(": {why}"));
        return Err(AiError::Refused(explanation));
    }
    Err(AiError::NoText)
}

fn parse_openai(value: &Value) -> Result<String, AiError> {
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"));
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return Ok(text.to_owned());
    }
    if let Some(refusal) = message
        .and_then(|m| m.get("refusal"))
        .and_then(Value::as_str)
    {
        return Err(AiError::Refused(format!(": {refusal}")));
    }
    Err(AiError::NoText)
}

fn parse_gemini(value: &Value) -> Result<String, AiError> {
    value
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or(AiError::NoText)
}

fn parse_ollama(value: &Value) -> Result<String, AiError> {
    value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or(AiError::NoText)
}

/// `(type, message)` of a provider error object, for both providers' shapes.
fn error_object(value: &Value) -> Option<(String, String)> {
    let error = value.get("error")?.as_object()?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_owned();
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_owned();
    Some((kind, message))
}

/// Human-readable message for a non-2xx body: the provider's error message
/// when the body is JSON, else a short excerpt.
pub fn error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some((kind, message)) = error_object(&value)
    {
        return format!("{kind}: {message}");
    }
    let excerpt: String = body.chars().take(ERROR_EXCERPT_CHARS).collect();
    excerpt.trim().to_owned()
}

/// Sends one question and returns the answer text (blocking).
pub fn ask(provider: Provider, api_key: &str, system: &str, user: &str) -> Result<String, AiError> {
    if provider.metadata().authentication.required() && api_key.trim().is_empty() {
        return Err(AiError::EmptyKey);
    }
    if user.trim().is_empty() {
        return Err(AiError::EmptyQuestion);
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let body = request_body(provider, system, user, DEFAULT_MAX_TOKENS).to_string();
    let mut request = agent.post(provider.endpoint());
    for (name, value) in headers(provider, api_key) {
        request = request.header(name, value);
    }
    let mut response = request.send(body)?;
    let code = response.status().as_u16();
    let text = response.body_mut().read_to_string()?;
    if !(200..300).contains(&code) {
        return Err(AiError::Status {
            code,
            message: error_message(&text),
        });
    }
    parse_response(provider, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC_OK: &str = r#"{"id":"msg_01XFDUDYJgAACzvnptvVoYEL","type":"message",
        "role":"assistant","model":"claude-sonnet-5",
        "content":[{"type":"thinking","thinking":"","signature":"EqQBCgIYAhIM"},
                   {"type":"text","text":"The paper targets 30 ms per 20-page chunk."},
                   {"type":"text","text":" See page 2."}],
        "stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":412,"output_tokens":18}}"#;
    const ANTHROPIC_REFUSAL: &str = r#"{"id":"msg_02","type":"message","role":"assistant",
        "model":"claude-sonnet-5","content":[],"stop_reason":"refusal",
        "stop_details":{"type":"refusal","category":"cyber","explanation":"policy"},
        "usage":{"input_tokens":10,"output_tokens":0}}"#;
    const ANTHROPIC_ERROR: &str = r#"{"type":"error","error":{"type":"authentication_error",
        "message":"invalid x-api-key"}}"#;
    const OPENAI_OK: &str = r#"{"id":"chatcmpl-abc123","object":"chat.completion","created":1,
        "model":"gpt-5","choices":[{"index":0,"message":{"role":"assistant",
        "content":"It reports 30 ms.","refusal":null},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":9,"completion_tokens":5,"total_tokens":14}}"#;
    const OPENAI_REFUSAL: &str = r#"{"id":"chatcmpl-def","object":"chat.completion",
        "choices":[{"index":0,"message":{"role":"assistant","content":null,
        "refusal":"I can't help with that."},"finish_reason":"stop"}]}"#;
    const OPENAI_ERROR: &str = r#"{"error":{"message":"Incorrect API key provided",
        "type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#;

    #[test]
    fn anthropic_body_matches_messages_api() {
        let body = request_body(Provider::Anthropic, "sys", "hello", 1234);
        assert_eq!(body["model"], "claude-sonnet-5");
        assert_eq!(body["max_tokens"], 1234);
        assert_eq!(body["system"], "sys");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("temperature").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn openai_body_matches_chat_completions() {
        let body = request_body(Provider::OpenAI, "sys", "hello", 1234);
        assert_eq!(body["model"], "gpt-5");
        assert!(body.get("max_tokens").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "sys");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
    }

    #[test]
    fn headers_carry_the_right_auth_scheme() {
        let anthropic = headers(Provider::Anthropic, "sk-test");
        assert!(anthropic.contains(&("content-type", "application/json".to_owned())));
        assert!(anthropic.contains(&("x-api-key", "sk-test".to_owned())));
        assert!(anthropic.contains(&("anthropic-version", "2023-06-01".to_owned())));
        assert!(anthropic.iter().all(|(name, _)| *name != "authorization"));

        let openai = headers(Provider::OpenAI, "sk-test");
        assert!(openai.contains(&("authorization", "Bearer sk-test".to_owned())));
        assert!(openai.iter().all(|(name, _)| *name != "x-api-key"));
    }

    #[test]
    fn anthropic_text_blocks_are_concatenated_skipping_thinking() {
        let text = parse_response(Provider::Anthropic, ANTHROPIC_OK).unwrap();
        assert_eq!(
            text,
            "The paper targets 30 ms per 20-page chunk. See page 2."
        );
    }

    #[test]
    fn anthropic_refusal_and_error_are_reported() {
        match parse_response(Provider::Anthropic, ANTHROPIC_REFUSAL) {
            Err(AiError::Refused(why)) => assert_eq!(why, ": policy"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_response(Provider::Anthropic, ANTHROPIC_ERROR) {
            Err(AiError::Api { kind, message }) => {
                assert_eq!(kind, "authentication_error");
                assert_eq!(message, "invalid x-api-key");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse_response(Provider::Anthropic, r#"{"type":"message","content":[]}"#),
            Err(AiError::NoText)
        ));
    }

    #[test]
    fn openai_content_refusal_and_error_are_reported() {
        assert_eq!(
            parse_response(Provider::OpenAI, OPENAI_OK).unwrap(),
            "It reports 30 ms."
        );
        match parse_response(Provider::OpenAI, OPENAI_REFUSAL) {
            Err(AiError::Refused(why)) => assert_eq!(why, ": I can't help with that."),
            other => panic!("unexpected {other:?}"),
        }
        match parse_response(Provider::OpenAI, OPENAI_ERROR) {
            Err(AiError::Api { kind, message }) => {
                assert_eq!(kind, "invalid_request_error");
                assert_eq!(message, "Incorrect API key provided");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse_response(Provider::OpenAI, r#"{"choices":[]}"#),
            Err(AiError::NoText)
        ));
    }

    #[test]
    fn non_json_bodies_are_errors_with_excerpts() {
        assert!(matches!(
            parse_response(Provider::Anthropic, "<html>bad gateway</html>"),
            Err(AiError::Json(_))
        ));
        assert_eq!(
            error_message(OPENAI_ERROR),
            "invalid_request_error: Incorrect API key provided"
        );
        assert_eq!(error_message("  plain text  "), "plain text");
        let long = "x".repeat(1000);
        assert_eq!(error_message(&long).chars().count(), ERROR_EXCERPT_CHARS);
    }

    #[test]
    fn ask_rejects_empty_key_and_question_before_any_io() {
        assert!(matches!(
            ask(Provider::Anthropic, "  ", "sys", "q"),
            Err(AiError::EmptyKey)
        ));
        assert!(matches!(
            ask(Provider::OpenAI, "key", "sys", "   "),
            Err(AiError::EmptyQuestion)
        ));
    }

    #[test]
    fn provider_registry_uses_canonical_services_and_environment_fallbacks() {
        let expected = [
            (Provider::OpenAI, ApiKeys::OPENAI, "OPENAI_API_KEY"),
            (Provider::Anthropic, ApiKeys::ANTHROPIC, "ANTHROPIC_API_KEY"),
            (Provider::Gemini, ApiKeys::GEMINI, "GEMINI_API_KEY"),
            (Provider::Ollama, ApiKeys::OLLAMA, "OLLAMA_API_KEY"),
        ];
        for (provider, service, environment) in expected {
            assert_eq!(provider.credential_service(), service);
            assert_eq!(
                crate::keys::EnvKeyProvider::env_var(service),
                Some(environment)
            );
        }
        assert_eq!(Provider::OpenAI.next(), Provider::Anthropic);
        assert_eq!(Provider::Ollama.next(), Provider::OpenAI);
        assert_eq!(PROVIDERS.len(), Provider::ALL.len());
    }

    #[test]
    fn gemini_and_ollama_protocol_shapes() {
        assert_eq!(
            request_body(Provider::Gemini, "sys", "hi", 10)["contents"][0]["parts"][0]["text"],
            "hi"
        );
        assert_eq!(
            request_body(Provider::Ollama, "sys", "hi", 10)["model"],
            OLLAMA_MODEL
        );
        assert!(headers(Provider::Gemini, "key").contains(&("x-goog-api-key", "key".to_owned())));
        assert_eq!(headers(Provider::Ollama, "").len(), 1);
        assert_eq!(
            parse_response(
                Provider::Gemini,
                r#"{"candidates":[{"content":{"parts":[{"text":"gem"}]}}]}"#
            )
            .unwrap(),
            "gem"
        );
        assert_eq!(
            parse_response(Provider::Ollama, r#"{"message":{"content":"local"}}"#).unwrap(),
            "local"
        );
        assert!(matches!(
            ask(Provider::Ollama, "", "sys", ""),
            Err(AiError::EmptyQuestion)
        ));
    }
}
