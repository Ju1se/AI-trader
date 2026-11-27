//! A chat model behind the OpenAI-compatible API: Ollama, vLLM or llama.cpp running next to
//! the server, or a hosted endpoint. The model only proposes; this client carries text in
//! both directions and decides nothing.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }

    fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub content: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Clone)]
pub struct ChatModel {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

impl ChatModel {
    /// `base_url` is the API root, for example `http://127.0.0.1:11434/v1` for Ollama.
    pub fn new(base_url: &str, model: &str, api_key: Option<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            // A local model on a small GPU can take minutes for a long prompt.
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.into(),
            api_key,
        })
    }

    pub fn name(&self) -> &str {
        &self.model
    }

    /// One reply, requested as a single JSON object at temperature 0.
    pub async fn complete_json(&self, messages: &[ChatMessage]) -> Result<Reply, String> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0,
            // Reasoning models think before they answer: leave room for both.
            "max_tokens": 4096,
            "response_format": {"type": "json_object"},
        });
        let mut request = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("model_unreachable:{e}"))?;
        let status = response.status();
        let value: Value = response
            .json()
            .await
            .map_err(|e| format!("model_bad_response:{e}"))?;
        if !status.is_success() {
            return Err(format!("model_error:{status}:{value}"));
        }
        let content = value["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| format!("model_bad_response:{value}"))?
            .to_string();
        let tokens = |key: &str| value["usage"][key].as_u64().unwrap_or(0);
        Ok(Reply {
            content,
            prompt_tokens: tokens("prompt_tokens"),
            completion_tokens: tokens("completion_tokens"),
        })
    }
}
