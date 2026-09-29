use super::{
    AiError, AnalysisRequest, AnalysisResponse, Provider, ProviderCapabilities, ProviderClient,
    error_object,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
pub const URL: &str = "https://api.openai.com/v1/chat/completions";
pub struct OpenAi;
pub static CLIENT: OpenAi = OpenAi;
impl ProviderClient for OpenAi {
    fn provider(&self) -> Provider {
        Provider::OpenAI
    }
    fn endpoint(&self, _: &str, _: &str) -> String {
        URL.into()
    }
    fn headers(&self, key: &str) -> Vec<(&'static str, String)> {
        vec![
            ("content-type", "application/json".into()),
            ("authorization", format!("Bearer {key}")),
        ]
    }
    fn build_request(&self, r: &AnalysisRequest) -> Result<Value, AiError> {
        let mut content = vec![json!({"type":"text","text":r.prompt})];
        for t in &r.context.text {
            content.push(json!({"type":"text","text":format!("[Page {}]\n{}",t.page.map_or_else(||"?".into(),|p|p.to_string()),t.text)}));
        }
        for i in &r.context.images {
            content.push(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",i.media_type,STANDARD.encode(&i.bytes))}}));
        }
        Ok(
            json!({"model":r.model,"max_completion_tokens":r.max_output_tokens,"messages":[{"role":"system","content":r.system},{"role":"user","content":content}]}),
        )
    }
    fn parse_response(&self, b: &str) -> Result<AnalysisResponse, AiError> {
        let v: Value = serde_json::from_str(b)?;
        if let Some((kind, message)) = error_object(&v) {
            return Err(AiError::Api { kind, message });
        }
        let m = v.pointer("/choices/0/message");
        if let Some(x) = m.and_then(|x| x.get("refusal")).and_then(Value::as_str) {
            return Err(AiError::Refused(format!(": {x}")));
        }
        let text = m
            .and_then(|x| x.get("content"))
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
            .ok_or(AiError::NoText)?;
        Ok(AnalysisResponse {
            text: text.into(),
            provenance: vec![],
            partial: v
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                == Some("length"),
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            multimodal: true,
            local: false,
            max_images: 500,
            supported_media_types: &["image/png", "image/jpeg", "image/webp", "image/gif"],
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixtures() {
        assert_eq!(
            CLIENT
                .parse_response(
                    r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"length"}]}"#
                )
                .unwrap()
                .text,
            "ok"
        );
        assert!(matches!(
            CLIENT.parse_response(r#"{"choices":[{"message":{"refusal":"no"}}]}"#),
            Err(AiError::Refused(_))
        ));
        assert!(matches!(
            CLIENT.parse_response(r#"{"choices":[]}"#),
            Err(AiError::NoText)
        ));
    }
}
