use anyhow::{Context, Result, bail};
use reqwest::{Client, multipart};
use serde::Deserialize;

use crate::config::{PasteConfig, PasteProvider};

#[derive(Clone)]
pub struct PasteClient {
    client: Client,
    config: PasteConfig,
    token: Option<String>,
}

impl PasteClient {
    pub fn new(config: &PasteConfig) -> Result<Self> {
        let token = match config.token_file.as_deref() {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read {}", path.display()))?
                    .trim()
                    .to_string(),
            ),
            None => None,
        };
        Ok(Self {
            client: Client::builder()
                .build()
                .context("failed to create paste HTTP client")?,
            config: config.clone(),
            token,
        })
    }

    pub fn should_paste(&self, text: &str) -> bool {
        self.config.enabled
            && self.config.provider != PasteProvider::None
            && text.chars().count() >= self.config.threshold_chars
    }

    pub async fn upload(&self, text: &str) -> Result<String> {
        match self.config.provider {
            PasteProvider::None => bail!("paste provider is disabled"),
            PasteProvider::PasteRs => {
                let url = if self.config.url.is_empty() {
                    "https://paste.rs"
                } else {
                    &self.config.url
                };
                let response = self
                    .client
                    .post(url)
                    .body(text.to_string())
                    .send()
                    .await?
                    .error_for_status()?;
                let body = response.text().await?;
                Ok(body.trim().to_string())
            }
            PasteProvider::ZeroZero => {
                let url = if self.config.url.is_empty() {
                    "https://0x0.st"
                } else {
                    &self.config.url
                };
                let part = multipart::Part::text(text.to_string())
                    .file_name("telagent-message.txt")
                    .mime_str("text/plain")?;
                let form = multipart::Form::new().part("file", part);
                let response = self
                    .client
                    .post(url)
                    .multipart(form)
                    .send()
                    .await?
                    .error_for_status()?;
                let body = response.text().await?;
                Ok(body.trim().to_string())
            }
            PasteProvider::Custom => {
                if self.config.url.is_empty() {
                    bail!("notify.paste.url is required for the custom provider");
                }
                let mut request = self
                    .client
                    .post(&self.config.url)
                    .body(text.to_string())
                    .header(reqwest::header::CONTENT_TYPE, "text/plain");
                if let Some(token) = &self.token {
                    request = request.bearer_auth(token);
                }
                let response = request.send().await?.error_for_status()?;
                let body = response.text().await?;
                if let Ok(parsed) = serde_json::from_str::<PasteResponse>(&body)
                    && let Some(url) = parsed.url
                {
                    return Ok(url);
                }
                Ok(body.trim().to_string())
            }
        }
    }
}

#[derive(Deserialize)]
struct PasteResponse {
    url: Option<String>,
}
