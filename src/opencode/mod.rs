use std::sync::Arc;

use anyhow::{Result, bail};

use crate::config::OpenCodeConfig;

pub mod backend;
pub mod types;
pub mod v1_http;
pub mod v2_http;

pub use backend::{AgentEvent, OpenCodeBackend};

pub fn build_backend(config: &OpenCodeConfig) -> Result<Arc<dyn OpenCodeBackend>> {
    match config.backend.as_str() {
        "v1" | "v1-http" => Ok(Arc::new(v1_http::V1Http::new(config)?)),
        "v2" | "v2-http" => Ok(Arc::new(v2_http::V2Http::new(config)?)),
        other => bail!("unsupported opencode backend {other:?}; supported: v1, v2"),
    }
}
