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

use serde_json::{Value, json};
use thiserror::Error;

use crate::keys::services;
use crate::transport::{self, BoundCredential, ProviderPolicy, ValidatedEndpoint};

/// Anthropic Messages endpoint.
pub const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
/// `OpenAI` chat completions endpoint.
pub const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
/// Required Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic model id used by the panel.
pub const ANTHROPIC_MODEL: &str = "claude-sonnet-5";
/// `OpenAI` model id used by the panel.
pub const OPENAI_MODEL: &str = "gpt-5";
/// Output token ceiling for a non-streaming answer in the panel.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
/// `User-Agent` sent with every request.
pub const USER_AGENT: &str = "tpe-app/0.1 (+https://github.com/benpshore/text-processing-engine)";

/// Which model provider answers the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic (Claude).
    Anthropic,
    /// `OpenAI` (`ChatGPT`).
    OpenAI,
}

impl Provider {
    /// Visible name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Claude",
            Self::OpenAI => "ChatGPT",
        }
    }

    /// Endpoint URL.
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Anthropic => ANTHROPIC_URL,
            Self::OpenAI => OPENAI_URL,
        }
    }

    /// Model id sent in the body.
    pub fn model(self) -> &'static str {
        match self {
            Self::Anthropic => ANTHROPIC_MODEL,
            Self::OpenAI => OPENAI_MODEL,
        }
    }

    /// Credential service name the API key is stored under.
    pub fn credential_service(self) -> &'static str {
        match self {
            Self::Anthropic => services::ANTHROPIC,
            Self::OpenAI => services::OPENAI,
        }
    }

    /// The other provider.
    #[must_use]
    pub fn toggle(self) -> Self {
        match self {
            Self::Anthropic => Self::OpenAI,
            Self::OpenAI => Self::Anthropic,
        }
    }
}

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
    /// Non-2xx HTTP status; the provider body is always redacted.
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

impl From<Provider> for ProviderPolicy {
    fn from(provider: Provider) -> Self {
        match provider {
            Provider::Anthropic => Self::Anthropic,
            Provider::OpenAI => Self::OpenAi,
        }
    }
}

/// Builds the JSON request body for `provider`.
pub fn request_body(provider: Provider, system: &str, user: &str, max_tokens: u32) -> Value {
    match provider {
        Provider::Anthropic => json!({
            "model": ANTHROPIC_MODEL,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        }),
        Provider::OpenAI => json!({
            "model": OPENAI_MODEL,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        }),
    }
}

/// Request headers for `provider`, including the authentication header.
#[cfg(test)]
pub fn headers(provider: Provider, api_key: &str) -> Vec<(&'static str, String)> {
    let mut out = vec![("content-type", "application/json".to_owned())];
    match provider {
        Provider::Anthropic => {
            out.push(("x-api-key", api_key.to_owned()));
            out.push(("anthropic-version", ANTHROPIC_VERSION.to_owned()));
        }
        Provider::OpenAI => out.push(("authorization", format!("Bearer {api_key}"))),
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

/// Human-readable message for a non-2xx body without exposing provider data.
pub fn error_message(body: &str) -> String {
    let _ = body;
    "provider returned an error (response body redacted)".to_owned()
}

/// Sends one question and returns the answer text (blocking).
pub fn ask(provider: Provider, api_key: &str, system: &str, user: &str) -> Result<String, AiError> {
    if api_key.trim().is_empty() {
        return Err(AiError::EmptyKey);
    }
    if user.trim().is_empty() {
        return Err(AiError::EmptyQuestion);
    }
    let body = request_body(provider, system, user, DEFAULT_MAX_TOKENS).to_string();
    let endpoint = ValidatedEndpoint::parse(provider.into(), provider.endpoint())
        .map_err(|error| AiError::Transport(error.to_string()))?;
    let credential = BoundCredential::new(&endpoint, api_key);
    let (code, text) =
        transport::post_json(&endpoint, &credential, &body).map_err(AiError::Transport)?;
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
            "provider returned an error (response body redacted)"
        );
        assert_eq!(
            error_message("secret body"),
            "provider returned an error (response body redacted)"
        );
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
    fn provider_table_is_consistent() {
        assert_eq!(Provider::Anthropic.toggle(), Provider::OpenAI);
        assert_eq!(Provider::OpenAI.toggle(), Provider::Anthropic);
        assert_ne!(
            Provider::Anthropic.credential_service(),
            Provider::OpenAI.credential_service()
        );
        assert!(
            Provider::Anthropic
                .endpoint()
                .starts_with("https://api.anthropic.com/")
        );
        assert!(
            Provider::OpenAI
                .endpoint()
                .starts_with("https://api.openai.com/")
        );
        assert_eq!(Provider::Anthropic.label(), "Claude");
        assert_eq!(Provider::OpenAI.label(), "ChatGPT");
    }
}
