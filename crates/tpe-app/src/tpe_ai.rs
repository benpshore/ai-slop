//! Provider-neutral, bounded multimodal document analysis.
//!
//! Provider wire formats live in sibling modules.  This module owns the types,
//! validation, transport, and the compatibility `ask` entry point used by the UI.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

use crate::keys::services;

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod openai;

pub const DEFAULT_MAX_TOKENS: u32 = 4096;
pub const USER_AGENT: &str = "tpe-app/0.1 (+https://github.com/benpshore/text-processing-engine)";
const ERROR_EXCERPT_CHARS: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    OpenAI,
    Gemini,
    Ollama,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Claude",
            Self::OpenAI => "ChatGPT",
            Self::Gemini => "Gemini",
            Self::Ollama => "Ollama",
        }
    }
    pub fn credential_service(self) -> &'static str {
        match self {
            Self::Anthropic => services::ANTHROPIC,
            Self::OpenAI => services::OPENAI,
            Self::Gemini => services::GEMINI,
            Self::Ollama => services::OLLAMA,
        }
    }
    #[must_use]
    pub fn toggle(self) -> Self {
        match self {
            Self::Anthropic => Self::OpenAI,
            Self::OpenAI => Self::Gemini,
            Self::Gemini => Self::Ollama,
            Self::Ollama => Self::Anthropic,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub document_hash: String,
    pub page: u32,
    pub artifact: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPart {
    pub text: String,
    pub page: Option<u32>,
    pub provenance: Option<Provenance>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePart {
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub page: u32,
    pub provenance: Provenance,
}

impl ImagePart {
    pub fn new(
        media_type: impl Into<String>,
        bytes: Vec<u8>,
        page: u32,
        provenance: Provenance,
        max_bytes: usize,
    ) -> Result<Self, AiError> {
        if bytes.len() > max_bytes {
            return Err(AiError::Limit(format!(
                "image on page {page} is {} bytes (limit {max_bytes})",
                bytes.len()
            )));
        }
        let media_type = media_type.into();
        if !matches!(
            media_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            return Err(AiError::UnsupportedMedia(media_type));
        }
        Ok(Self {
            media_type,
            bytes,
            page,
            provenance,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentContext {
    pub document_hash: String,
    pub text: Vec<TextPart>,
    pub images: Vec<ImagePart>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisLimits {
    pub max_images: usize,
    pub max_image_bytes: usize,
    pub max_total_bytes: usize,
    pub max_pages: usize,
    pub max_output_tokens: u32,
}
impl Default for AnalysisLimits {
    fn default() -> Self {
        Self {
            max_images: 8,
            max_image_bytes: 5 * 1024 * 1024,
            max_total_bytes: 20 * 1024 * 1024,
            max_pages: 20,
            max_output_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisRequest {
    pub system: String,
    pub prompt: String,
    pub model: String,
    pub max_output_tokens: u32,
    pub context: DocumentContext,
}

impl AnalysisRequest {
    pub fn validate(&self, limits: AnalysisLimits) -> Result<(), AiError> {
        if self.prompt.trim().is_empty() {
            return Err(AiError::EmptyQuestion);
        }
        if self.max_output_tokens == 0 || self.max_output_tokens > limits.max_output_tokens {
            return Err(AiError::Limit("output token limit exceeded".into()));
        }
        if self.context.images.len() > limits.max_images {
            return Err(AiError::Limit("image count limit exceeded".into()));
        }
        if self
            .context
            .images
            .iter()
            .any(|i| i.bytes.len() > limits.max_image_bytes)
        {
            return Err(AiError::Limit("per-image byte limit exceeded".into()));
        }
        let mut pages = std::collections::BTreeSet::new();
        for text in &self.context.text {
            if let Some(page) = text.page {
                pages.insert(page);
            }
        }
        for image in &self.context.images {
            pages.insert(image.page);
        }
        if pages.len() > limits.max_pages {
            return Err(AiError::Limit("page count limit exceeded".into()));
        }
        let bytes = self.system.len()
            + self.prompt.len()
            + self
                .context
                .text
                .iter()
                .map(|p| p.text.len())
                .sum::<usize>()
            + self
                .context
                .images
                .iter()
                .map(|p| p.bytes.len())
                .sum::<usize>();
        if bytes > limits.max_total_bytes {
            return Err(AiError::Limit("total request byte limit exceeded".into()));
        }
        Ok(())
    }
    pub fn pages(&self) -> Vec<u32> {
        let mut p = std::collections::BTreeSet::new();
        for x in &self.context.text {
            if let Some(n) = x.page {
                p.insert(n);
            }
        }
        for x in &self.context.images {
            p.insert(x.page);
        }
        p.into_iter().collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisResponse {
    pub text: String,
    pub provenance: Vec<Provenance>,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub multimodal: bool,
    pub local: bool,
    pub max_images: usize,
    pub supported_media_types: &'static [&'static str],
}

pub trait ProviderClient: Sync {
    fn provider(&self) -> Provider;
    fn endpoint(&self, model: &str, api_key: &str) -> String;
    fn headers(&self, api_key: &str) -> Vec<(&'static str, String)>;
    fn build_request(&self, request: &AnalysisRequest) -> Result<Value, AiError>;
    fn parse_response(&self, body: &str) -> Result<AnalysisResponse, AiError>;
    fn timeout(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn capabilities(&self) -> ProviderCapabilities;
}

pub fn client(provider: Provider) -> &'static dyn ProviderClient {
    match provider {
        Provider::Anthropic => &anthropic::CLIENT,
        Provider::OpenAI => &openai::CLIENT,
        Provider::Gemini => &gemini::CLIENT,
        Provider::Ollama => &ollama::CLIENT,
    }
}

#[derive(Debug, Error)]
pub enum AiError {
    #[error("no API key supplied")]
    EmptyKey,
    #[error("the question is empty")]
    EmptyQuestion,
    #[error("request limit: {0}")]
    Limit(String),
    #[error("unsupported image media type: {0}")]
    UnsupportedMedia(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("HTTP {code}: {message}")]
    Status { code: u16, message: String },
    #[error("response is not JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("API error ({kind}): {message}")]
    Api { kind: String, message: String },
    #[error("the model declined to answer{0}")]
    Refused(String),
    #[error("response contains no text")]
    NoText,
}
impl From<ureq::Error> for AiError {
    fn from(e: ureq::Error) -> Self {
        Self::Transport(e.to_string())
    }
}

pub(crate) fn error_object(value: &Value) -> Option<(String, String)> {
    let error = value.get("error")?.as_object()?;
    Some((
        error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_owned(),
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned(),
    ))
}
pub fn error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body)
        && let Some((k, m)) = error_object(&v)
    {
        return format!("{k}: {m}");
    }
    body.chars()
        .take(ERROR_EXCERPT_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub fn analyze(
    provider: Provider,
    api_key: &str,
    request: &AnalysisRequest,
    limits: AnalysisLimits,
) -> Result<AnalysisResponse, AiError> {
    let adapter = client(provider);
    request.validate(limits)?;
    if !adapter.capabilities().local && api_key.trim().is_empty() {
        return Err(AiError::EmptyKey);
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(adapter.timeout()))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build();
    let body = adapter.build_request(request)?.to_string();
    if body.len() > limits.max_total_bytes * 2 {
        return Err(AiError::Limit("encoded request byte limit exceeded".into()));
    }
    let agent = ureq::Agent::new_with_config(config);
    let endpoint = adapter.endpoint(&request.model, api_key);
    let mut wire = agent.post(&endpoint);
    for (name, value) in adapter.headers(api_key) {
        wire = wire.header(name, value);
    }
    let mut response = wire.send(body)?;
    let code = response.status().as_u16();
    let text = response.body_mut().read_to_string()?;
    if !(200..300).contains(&code) {
        return Err(AiError::Status {
            code,
            message: error_message(&text),
        });
    }
    let mut parsed = adapter.parse_response(&text)?;
    if parsed.provenance.is_empty() {
        parsed.provenance = request
            .pages()
            .into_iter()
            .map(|page| Provenance {
                document_hash: request.context.document_hash.clone(),
                page,
                artifact: None,
            })
            .collect();
    }
    Ok(parsed)
}

/// Backwards-compatible text-only entry point.
pub fn ask(provider: Provider, api_key: &str, system: &str, user: &str) -> Result<String, AiError> {
    let request = AnalysisRequest {
        system: system.into(),
        prompt: user.into(),
        model: default_model(provider).into(),
        max_output_tokens: DEFAULT_MAX_TOKENS,
        context: DocumentContext::default(),
    };
    analyze(provider, api_key, &request, AnalysisLimits::default()).map(|r| r.text)
}
pub fn default_model(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "claude-sonnet-5",
        Provider::OpenAI => "gpt-5",
        Provider::Gemini => "gemini-2.5-pro",
        Provider::Ollama => "llava",
    }
}
pub fn request_body(provider: Provider, system: &str, user: &str, max_tokens: u32) -> Value {
    client(provider)
        .build_request(&AnalysisRequest {
            system: system.into(),
            prompt: user.into(),
            model: default_model(provider).into(),
            max_output_tokens: max_tokens,
            context: DocumentContext::default(),
        })
        .expect("text request serializes")
}
pub fn headers(provider: Provider, key: &str) -> Vec<(&'static str, String)> {
    client(provider).headers(key)
}
pub fn parse_response(provider: Provider, body: &str) -> Result<String, AiError> {
    client(provider).parse_response(body).map(|r| r.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn multimodal() -> AnalysisRequest {
        AnalysisRequest {
            system: "cite pages".into(),
            prompt: "summarize".into(),
            model: "test".into(),
            max_output_tokens: 100,
            context: DocumentContext {
                document_hash: "abc".into(),
                text: vec![TextPart {
                    text: "page text".into(),
                    page: Some(2),
                    provenance: Some(Provenance {
                        document_hash: "abc".into(),
                        page: 2,
                        artifact: None,
                    }),
                }],
                images: vec![
                    ImagePart::new(
                        "image/png",
                        vec![1, 2, 3],
                        2,
                        Provenance {
                            document_hash: "abc".into(),
                            page: 2,
                            artifact: Some("figure-1".into()),
                        },
                        10,
                    )
                    .unwrap(),
                ],
            },
        }
    }
    #[test]
    fn all_serializers_accept_multimodal_fixture() {
        for p in [
            Provider::OpenAI,
            Provider::Anthropic,
            Provider::Gemini,
            Provider::Ollama,
        ] {
            let v = client(p).build_request(&multimodal()).unwrap();
            assert!(v.to_string().contains("AQID"));
        }
    }
    #[test]
    fn limits_apply_before_transport() {
        let mut r = multimodal();
        r.max_output_tokens = 101;
        assert!(matches!(
            r.validate(AnalysisLimits {
                max_output_tokens: 100,
                ..AnalysisLimits::default()
            }),
            Err(AiError::Limit(_))
        ));
    }
    #[test]
    fn malformed_is_reported_by_every_parser() {
        for p in [
            Provider::OpenAI,
            Provider::Anthropic,
            Provider::Gemini,
            Provider::Ollama,
        ] {
            assert!(matches!(
                client(p).parse_response("{"),
                Err(AiError::Json(_))
            ));
        }
    }
}
