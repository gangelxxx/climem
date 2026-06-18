//! The `api` provider: neural embeddings over HTTP (desc.md §5). It works with
//! OpenAI-compatible and Ollama embedding endpoints. The API key always comes
//! from the environment (whatever var `embedding.api_key_env` names), never from
//! the config file.

use super::Embedder;
use crate::config::Config;
use crate::util::{AppError, Result};
use serde_json::{json, Value};

pub struct ApiEmbedder {
    endpoint: String,
    model: String,
    format: String,
    api_key: Option<String>,
    #[allow(dead_code)] // only read back out through Embedder::dim
    dim: usize,
}

impl ApiEmbedder {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let endpoint = cfg.embedding.endpoint.clone().ok_or_else(|| {
            AppError::with_hint(
                "embedding.endpoint is not set for provider \"api\"",
                "cm config set embedding.endpoint https://api.openai.com/v1/embeddings",
            )
        })?;
        let api_key = std::env::var(&cfg.embedding.api_key_env)
            .ok()
            .filter(|s| !s.is_empty());
        Ok(ApiEmbedder {
            endpoint,
            model: cfg.embedding.model.clone(),
            format: cfg.embedding.api_format.clone(),
            api_key,
            dim: cfg.embedding.dimension,
        })
    }

    fn request(&self, text: &str) -> Result<Vec<f32>> {
        let body = match self.format.as_str() {
            "ollama" => json!({ "model": self.model, "prompt": text }),
            _ => json!({ "model": self.model, "input": text }),
        };

        let mut req = ureq::post(&self.endpoint).set("Content-Type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }

        let resp: Value = req
            .send_json(body)
            .map_err(|e| {
                AppError::with_hint(
                    format!("embedding API request failed: {e}"),
                    "Check embedding.endpoint, the key env var, and that the service is reachable.",
                )
            })?
            .into_json()
            .map_err(|e| AppError::new(format!("embedding API returned non-JSON: {e}")))?;

        extract_vector(&resp, &self.format)
    }
}

fn extract_vector(resp: &Value, format: &str) -> Result<Vec<f32>> {
    let arr = match format {
        // Ollama: {"embedding":[...]}
        "ollama" => resp.get("embedding"),
        // OpenAI: {"data":[{"embedding":[...]}]}
        _ => resp
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|e| e.get("embedding")),
    }
    .and_then(|v| v.as_array())
    .ok_or_else(|| AppError::new("could not find an embedding array in the API response"))?;

    Ok(arr
        .iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect())
}

impl Embedder for ApiEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.request(text)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn signature(&self) -> String {
        format!("api:{}:{}", self.model, self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn approx(got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len(), "length mismatch");
        for (a, b) in got.iter().zip(want) {
            assert!((a - b).abs() <= 1e-6, "{a} != {b}");
        }
    }

    #[test]
    fn api_extract_vector_openai_shape() {
        let resp = json!({"data": [{"embedding": [0.1, 0.2, 0.3]}]});
        approx(&extract_vector(&resp, "openai").unwrap(), &[0.1, 0.2, 0.3]);
    }

    #[test]
    fn api_extract_vector_ollama_shape() {
        let resp = json!({"embedding": [0.5, 0.6]});
        approx(&extract_vector(&resp, "ollama").unwrap(), &[0.5, 0.6]);
    }

    #[test]
    fn api_extract_vector_missing_array_errors() {
        assert!(extract_vector(&json!({"foo": 1}), "openai")
            .unwrap_err()
            .msg
            .contains("could not find an embedding array"));
        // Empty data array -> no element 0 -> error.
        assert!(extract_vector(&json!({"data": []}), "openai").is_err());
    }

    #[test]
    fn api_extract_vector_skips_non_numeric() {
        let resp = json!({"embedding": [0.1, "x", null, 0.2]});
        approx(&extract_vector(&resp, "ollama").unwrap(), &[0.1, 0.2]);
    }

    #[test]
    fn api_from_config_missing_endpoint_errors() {
        // ApiEmbedder holds the key, so it has no Debug -> match instead of unwrap_err.
        let err = match ApiEmbedder::from_config(&Config::default()) {
            Ok(_) => panic!("expected missing-endpoint error"),
            Err(e) => e,
        };
        assert!(err.msg.contains("endpoint is not set"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn api_from_config_reads_key_from_env() {
        let mut cfg = Config::default();
        cfg.embedding.endpoint = Some("http://127.0.0.1:0/x".into());
        cfg.embedding.api_key_env = "CLIMEM_TEST_KEY_PRESENT".into();
        std::env::set_var("CLIMEM_TEST_KEY_PRESENT", "secret-value");
        let emb = ApiEmbedder::from_config(&cfg).unwrap();
        std::env::remove_var("CLIMEM_TEST_KEY_PRESENT");
        assert_eq!(emb.api_key.as_deref(), Some("secret-value"));

        // Empty value is filtered to None.
        cfg.embedding.api_key_env = "CLIMEM_TEST_KEY_EMPTY".into();
        std::env::set_var("CLIMEM_TEST_KEY_EMPTY", "");
        let emb2 = ApiEmbedder::from_config(&cfg).unwrap();
        std::env::remove_var("CLIMEM_TEST_KEY_EMPTY");
        assert_eq!(emb2.api_key, None);
    }

    #[test]
    fn api_signature_format() {
        let mut cfg = Config::default();
        cfg.embedding.endpoint = Some("http://127.0.0.1:0/x".into());
        let emb = ApiEmbedder::from_config(&cfg).unwrap();
        assert_eq!(emb.signature(), "api:hash-ngram-v1:openai");
    }
}
