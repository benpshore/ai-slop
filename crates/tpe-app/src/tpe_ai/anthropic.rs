use super::{
    AiError, AnalysisRequest, AnalysisResponse, Provider, ProviderCapabilities, ProviderClient,
    error_object,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
pub const URL: &str = "https://api.anthropic.com/v1/messages";
pub const VERSION: &str = "2023-06-01";
pub struct Anthropic;
pub static CLIENT: Anthropic = Anthropic;
impl ProviderClient for Anthropic {
    fn provider(&self) -> Provider {
        Provider::Anthropic
    }
    fn endpoint(&self, _: &str, _: &str) -> String {
        URL.into()
    }
    fn headers(&self, k: &str) -> Vec<(&'static str, String)> {
        vec![
            ("content-type", "application/json".into()),
            ("x-api-key", k.into()),
            ("anthropic-version", VERSION.into()),
        ]
    }
    fn build_request(&self, r: &AnalysisRequest) -> Result<Value, AiError> {
        let mut c = vec![json!({"type":"text","text":r.prompt})];
        for t in &r.context.text {
            c.push(json!({"type":"text","text":format!("[Page {}]\n{}",t.page.map_or_else(||"?".into(),|p|p.to_string()),t.text)}));
        }
        for i in &r.context.images {
            c.push(json!({"type":"image","source":{"type":"base64","media_type":i.media_type,"data":STANDARD.encode(&i.bytes)}}));
        }
        Ok(
            json!({"model":r.model,"max_tokens":r.max_output_tokens,"system":r.system,"messages":[{"role":"user","content":c}]}),
        )
    }
    fn parse_response(&self, b: &str) -> Result<AnalysisResponse, AiError> {
        let v: Value = serde_json::from_str(b)?;
        if let Some((kind, message)) = error_object(&v) {
            return Err(AiError::Api { kind, message });
        }
        let text = v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|x| x.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|x| x.get("text").and_then(Value::as_str))
            .collect::<String>();
        if text.is_empty() {
            if v.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
                return Err(AiError::Refused(String::new()));
            }
            return Err(AiError::NoText);
        }
        Ok(AnalysisResponse {
            text,
            provenance: vec![],
            partial: v.get("stop_reason").and_then(Value::as_str) == Some("max_tokens"),
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            multimodal: true,
            local: false,
            max_images: 100,
            supported_media_types: &["image/png", "image/jpeg", "image/webp", "image/gif"],
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures() {
        assert_eq!(CLIENT.parse_response(r#"{"content":[{"type":"thinking"},{"type":"text","text":"a"},{"type":"text","text":"b"}],"stop_reason":"max_tokens"}"#).unwrap().text,"ab");
        assert!(matches!(
            CLIENT.parse_response(r#"{"content":[],"stop_reason":"refusal"}"#),
            Err(AiError::Refused(_))
        ));
        assert!(matches!(
            CLIENT.parse_response(r#"{"content":[]}"#),
            Err(AiError::NoText)
        ));
    }
}
