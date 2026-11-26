use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub instance_id: Option<String>,
    pub telegram: TelegramConfig,
    pub opencode: OpenCodeConfig,
    pub notify: NotifyConfig,
    pub paste: PasteConfig,
    pub storage: StorageConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct TelegramConfig {
    pub bot_token_file: Option<PathBuf>,
    pub allowed_user_ids: Vec<i64>,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenCodeConfig {
    pub backend: String,
    pub url: String,
    pub username: String,
    pub password_file: Option<PathBuf>,
    pub events_path: String,
    pub auto_discover: bool,
    pub service_file: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub health_interval_secs: u64,
    pub health_timeout_secs: u64,
    pub session_stale_secs: u64,
    pub event_idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub on_idle: bool,
    pub on_error: bool,
    pub on_created: bool,
    pub permissions: bool,
    pub questions: bool,
    pub child_sessions: bool,
    pub include_last_message: bool,
    pub max_telegram_chars: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PasteConfig {
    pub enabled: bool,
    pub url: String,
    pub threshold_chars: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct StorageConfig {
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub pairing_ttl_secs: u64,
    pub require_bind: bool,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            backend: "v1".to_string(),
            url: "http://127.0.0.1:4096".to_string(),
            username: "opencode".to_string(),
            password_file: None,
            events_path: "/global/event".to_string(),
            auto_discover: false,
            service_file: None,
            directory: None,
            health_interval_secs: 30,
            health_timeout_secs: 10,
            session_stale_secs: 900,
            event_idle_timeout_secs: 120,
        }
    }
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            on_idle: true,
            on_error: true,
            on_created: false,
            permissions: true,
            questions: true,
            child_sessions: false,
            include_last_message: true,
            max_telegram_chars: 3900,
        }
    }
}

impl Default for PasteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "https://cocoon.sereph.us".to_string(),
            threshold_chars: 12_000,
            max_bytes: 4 * 1024 * 1024,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            pairing_ttl_secs: 600,
            require_bind: true,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut config = if let Some(path) = path {
            let text = fs::read_to_string(path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("failed to parse config {}", path.display()))?
        } else {
            Self::default()
        };

        if let Ok(value) = env::var("TELAGENT_OPENCODE_URL") {
            config.opencode.url = value;
        }
        if let Ok(value) = env::var("TELAGENT_OPENCODE_USERNAME") {
            config.opencode.username = value;
        }
        if let Ok(value) = env::var("TELAGENT_OPENCODE_PASSWORD_FILE") {
            config.opencode.password_file = Some(PathBuf::from(value));
        }
        if let Ok(value) = env::var("TELAGENT_OPENCODE_AUTO_DISCOVER") {
            config.opencode.auto_discover = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Ok(value) = env::var("TELAGENT_OPENCODE_SERVICE_FILE") {
            config.opencode.service_file = Some(PathBuf::from(value));
        }
        if let Ok(value) = env::var("TELAGENT_BOT_TOKEN_FILE") {
            config.telegram.bot_token_file = Some(PathBuf::from(value));
        }
        if let Ok(value) = env::var("TELEGRAM_PROXY") {
            config.telegram.proxy = Some(value);
        }
        if let Ok(value) = env::var("TELAGENT_STATE_PATH") {
            config.storage.path = Some(PathBuf::from(value));
        } else if let Ok(value) = env::var("TELAGENT_STATE_DB") {
            config.storage.path = Some(PathBuf::from(value));
        }
        if let Ok(value) = env::var("TELAGENT_INSTANCE_ID") {
            config.instance_id = Some(value);
        }

        Ok(config)
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("telagent")
            .join("config.toml")
    }

    pub fn state_path(&self) -> PathBuf {
        self.storage.path.clone().unwrap_or_else(|| {
            dirs::state_dir()
                .or_else(dirs::data_local_dir)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("telagent")
                .join("state.redb")
        })
    }

    pub fn bot_token(&self) -> Result<String> {
        if let Ok(value) = env::var("TELAGENT_TELEGRAM_BOT_TOKEN") {
            return Ok(value);
        }
        let path = self
            .telegram
            .bot_token_file
            .as_deref()
            .context("telegram.bot_token_file is not configured")?;
        read_secret(path).context("failed to read telegram bot token")
    }
}

fn read_secret(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        assert_eq!(config.opencode.url, "http://127.0.0.1:4096");
        assert_eq!(config.opencode.health_interval_secs, 30);
        assert_eq!(config.opencode.health_timeout_secs, 10);
        assert_eq!(config.opencode.session_stale_secs, 900);
        assert_eq!(config.opencode.event_idle_timeout_secs, 120);
        assert_eq!(config.notify.max_telegram_chars, 3900);
        assert_eq!(config.paste.max_bytes, 4 * 1024 * 1024);
        assert!(config.security.require_bind);
    }

    #[test]
    fn parses_partial_config() {
        let config: Config = toml::from_str(
            r#"
            [telegram]
            allowed_user_ids = [1, 2]

            [paste]
            enabled = true
            url = "https://paste.example.com"
            "#,
        )
        .unwrap();

        assert_eq!(config.telegram.allowed_user_ids, vec![1, 2]);
        assert!(config.paste.enabled);
        assert_eq!(config.paste.url, "https://paste.example.com");
    }
}
