use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::PasteConfig;

#[derive(Clone)]
pub struct PasteClient {
    client: Client,
    config: PasteConfig,
}

impl PasteClient {
    pub fn new(config: &PasteConfig) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .build()
                .context("failed to create paste HTTP client")?,
            config: config.clone(),
        })
    }

    pub fn should_paste(&self, text: &str) -> bool {
        self.enabled() && text.chars().count() >= self.config.threshold_chars
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled && !self.config.url.trim().is_empty()
    }

    pub async fn upload(
        &self,
        text: &str,
        title: &str,
        publish_at: Option<&str>,
    ) -> Result<String> {
        if !self.config.enabled {
            bail!("paste is disabled; set paste.enabled = true");
        }
        let url = self.config.url.trim();
        if url.is_empty() {
            bail!("paste.url is not configured");
        }

        #[derive(Serialize)]
        struct Body<'a> {
            content: &'a str,
            title: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            publish_at: Option<&'a str>,
            private: bool,
        }

        let response = self
            .client
            .post(format!("{}/api/paste", url.trim_end_matches('/')))
            .json(&Body {
                content: text,
                title,
                publish_at: publish_at.filter(|value| !value.trim().is_empty()),
                private: true,
            })
            .send()
            .await?
            .error_for_status()
            .context("cocoon paste upload failed")?;

        let paste: PasteResponse = response
            .json()
            .await
            .context("failed to decode cocoon paste response")?;
        if paste.url.trim().is_empty() {
            bail!("cocoon paste response did not include a url");
        }
        Ok(paste.url)
    }
}

#[derive(Deserialize)]
struct PasteResponse {
    url: String,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::{Value, json};

    use super::*;

    #[tokio::test]
    async fn uploads_content_title_and_reveal_time() {
        let captured = Arc::new(Mutex::new(None));
        let state = captured.clone();
        let router =
            Router::new()
                .route(
                    "/api/paste",
                    post(
                        |State(captured): State<Arc<Mutex<Option<Value>>>>,
                         Json(body): Json<Value>| async move {
                            *captured.lock().unwrap() = Some(body);
                            Json(json!({"url": "https://paste.example.com/p/abc123"}))
                        },
                    ),
                )
                .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = PasteClient::new(&PasteConfig {
            enabled: true,
            url: format!("http://{address}/"),
            ..Default::default()
        })
        .unwrap();
        let url = client
            .upload("history", "Session title", Some("2030-01-01T00:00:00Z"))
            .await
            .unwrap();

        assert_eq!(url, "https://paste.example.com/p/abc123");
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["content"], "history");
        assert_eq!(body["title"], "Session title");
        assert_eq!(body["publish_at"], "2030-01-01T00:00:00Z");
        assert_eq!(body["private"], true);
    }
}
