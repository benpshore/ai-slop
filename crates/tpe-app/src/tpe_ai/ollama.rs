use super::{
    AiError, AnalysisRequest, AnalysisResponse, Provider, ProviderCapabilities, ProviderClient,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
pub struct Ollama;
pub static CLIENT: Ollama = Ollama;
impl ProviderClient for Ollama {
    fn provider(&self) -> Provider {
        Provider::Ollama
    }
    fn endpoint(&self, _: &str, _: &str) -> String {
        "http://127.0.0.1:11434/api/chat".into()
    }
    fn headers(&self, _: &str) -> Vec<(&'static str, String)> {
        vec![("content-type", "application/json".into())]
    }
    fn build_request(&self, r: &AnalysisRequest) -> Result<Value, AiError> {
        let mut text = r.prompt.clone();
        for t in &r.context.text {
            text.push_str(&format!(
                "\n\n[Page {}]\n{}",
                t.page.map_or_else(|| "?".into(), |x| x.to_string()),
                t.text
            ));
        }
        let images: Vec<_> = r
            .context
            .images
            .iter()
            .map(|i| STANDARD.encode(&i.bytes))
            .collect();
        Ok(
            json!({"model":r.model,"stream":false,"options":{"num_predict":r.max_output_tokens},"messages":[{"role":"system","content":r.system},{"role":"user","content":text,"images":images}]}),
        )
    }
    fn parse_response(&self, b: &str) -> Result<AnalysisResponse, AiError> {
        let v: Value = serde_json::from_str(b)?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            return Err(AiError::Api {
                kind: "ollama".into(),
                message: e.into(),
            });
        }
        let text = v
            .pointer("/message/content")
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
            .ok_or(AiError::NoText)?;
        Ok(AnalysisResponse {
            text: text.into(),
            provenance: vec![],
            partial: !v.get("done").and_then(Value::as_bool).unwrap_or(false),
        })
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            multimodal: true,
            local: true,
            max_images: 16,
            supported_media_types: &["image/png", "image/jpeg", "image/webp"],
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
                .parse_response(r#"{"message":{"content":"ok"},"done":true}"#)
                .unwrap()
                .text,
            "ok"
        );
        assert!(
            CLIENT
                .parse_response(r#"{"message":{"content":"part"},"done":false}"#)
                .unwrap()
                .partial
        );
        assert!(matches!(
            CLIENT.parse_response(r#"{"error":"missing model"}"#),
            Err(AiError::Api { .. })
        ));
    }
}
