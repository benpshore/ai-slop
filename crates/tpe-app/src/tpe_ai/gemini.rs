use super::{
    AiError, AnalysisRequest, AnalysisResponse, Provider, ProviderCapabilities, ProviderClient,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
pub struct Gemini;
pub static CLIENT: Gemini = Gemini;
impl ProviderClient for Gemini {
    fn provider(&self) -> Provider {
        Provider::Gemini
    }
    fn endpoint(&self, m: &str, k: &str) -> String {
        format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{m}:generateContent?key={k}"
        )
    }
    fn headers(&self, _: &str) -> Vec<(&'static str, String)> {
        vec![("content-type", "application/json".into())]
    }
    fn build_request(&self, r: &AnalysisRequest) -> Result<Value, AiError> {
        let mut p = vec![json!({"text":r.prompt})];
        for t in &r.context.text {
            p.push(json!({"text":format!("[Page {}]\n{}",t.page.map_or_else(||"?".into(),|x|x.to_string()),t.text)}));
        }
        for i in &r.context.images {
            p.push(
                json!({"inline_data":{"mime_type":i.media_type,"data":STANDARD.encode(&i.bytes)}}),
            );
        }
        Ok(
            json!({"system_instruction":{"parts":[{"text":r.system}]},"contents":[{"role":"user","parts":p}],"generationConfig":{"maxOutputTokens":r.max_output_tokens}}),
        )
    }
    fn parse_response(&self, b: &str) -> Result<AnalysisResponse, AiError> {
        let v: Value = serde_json::from_str(b)?;
        if let Some(reason) = v
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            return Err(AiError::Refused(format!(": {reason}")));
        }
        let text = v
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<String>();
        if text.is_empty() {
            return Err(AiError::NoText);
        }
        Ok(AnalysisResponse {
            text,
            provenance: vec![],
            partial: v
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str)
                == Some("MAX_TOKENS"),
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            multimodal: true,
            local: false,
            max_images: 3000,
            supported_media_types: &["image/png", "image/jpeg", "image/webp"],
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures() {
        assert_eq!(CLIENT.parse_response(r#"{"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}]}"#).unwrap().text,"partial");
        assert!(matches!(
            CLIENT.parse_response(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#),
            Err(AiError::Refused(_))
        ));
        assert!(matches!(CLIENT.parse_response("{}"), Err(AiError::NoText)));
    }
}
