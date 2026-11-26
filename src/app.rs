use std::sync::Arc;

use anyhow::Result;

use crate::{
    config::Config,
    metrics::RuntimeStatus,
    notifier::Notifier,
    opencode::{OpenCodeBackend, types::SessionInfo},
    paste::PasteClient,
    state::{Db, SessionRecord},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub instance_id: String,
    pub db: Db,
    pub backend: Arc<dyn OpenCodeBackend>,
    pub notifier: Notifier,
    pub paste: PasteClient,
    pub runtime: Arc<RuntimeStatus>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        instance_id: String,
        db: Db,
        backend: Arc<dyn OpenCodeBackend>,
        notifier: Notifier,
        paste: PasteClient,
        runtime: Arc<RuntimeStatus>,
    ) -> Self {
        Self {
            config,
            instance_id,
            db,
            backend,
            notifier,
            paste,
            runtime,
        }
    }

    pub fn is_allowed_user(&self, user_id: u64) -> bool {
        self.config.telegram.allowed_user_ids.is_empty()
            || self
                .config
                .telegram
                .allowed_user_ids
                .contains(&(user_id as i64))
    }

    pub async fn session_directory(&self, session_id: &str) -> Result<String> {
        if let Some(session) = self.db.get_session(&self.instance_id, session_id).await?
            && !session.directory.is_empty()
        {
            return Ok(session.directory);
        }
        let Some(info) = self.backend.get_session(session_id).await? else {
            return Ok(String::new());
        };
        let session = self.sync_session(&info, "").await?;
        Ok(session.directory)
    }

    pub async fn forget_pending_request(&self, request_id: &str) -> Result<()> {
        match self.db.list_pending_requests().await {
            Ok(requests) => {
                for request in requests
                    .into_iter()
                    .filter(|request| request.request_id == request_id)
                {
                    self.notifier
                        .delete_telegram_message(request.chat_id, request.message_id)
                        .await;
                }
            }
            Err(error) => {
                tracing::warn!(%error, request_id, "failed to list pending requests");
            }
        }
        self.db.remove_pending_request(request_id).await
    }

    pub async fn sync_session(&self, info: &SessionInfo, state: &str) -> Result<SessionRecord> {
        let existing = self.db.get_session(&self.instance_id, &info.id).await?;

        let record = SessionRecord {
            instance_id: self.instance_id.clone(),
            session_id: info.id.clone(),
            title: if info.title.trim().is_empty() {
                existing
                    .as_ref()
                    .map(|session| session.title.clone())
                    .unwrap_or_default()
            } else {
                info.title.clone()
            },
            directory: if info.directory.trim().is_empty() {
                existing
                    .as_ref()
                    .map(|session| session.directory.clone())
                    .unwrap_or_default()
            } else {
                info.directory.clone()
            },
            project_id: info
                .project_id
                .clone()
                .or_else(|| existing.as_ref().and_then(|s| s.project_id.clone())),
            parent_id: info
                .parent_id
                .clone()
                .or_else(|| existing.as_ref().and_then(|s| s.parent_id.clone())),
            state: if state.is_empty() {
                existing
                    .as_ref()
                    .map(|session| session.state.clone())
                    .unwrap_or_else(|| "idle".to_string())
            } else {
                state.to_string()
            },
            last_assistant_message_id: existing
                .as_ref()
                .and_then(|s| s.last_assistant_message_id.clone()),
            last_notified_message_id: existing
                .as_ref()
                .and_then(|s| s.last_notified_message_id.clone()),
            notification_pending: existing
                .as_ref()
                .map(|s| s.notification_pending)
                .unwrap_or(false),
            muted: existing.as_ref().map(|s| s.muted).unwrap_or(false),
            updated_at: existing.as_ref().map(|s| s.updated_at).unwrap_or(0),
        };

        self.db.upsert_session(&record).await?;
        Ok(record)
    }

    pub async fn get_or_placeholder_session(&self, session_id: &str) -> Result<SessionRecord> {
        if let Some(session) = self.db.get_session(&self.instance_id, session_id).await? {
            return Ok(session);
        }
        let session = SessionRecord {
            instance_id: self.instance_id.clone(),
            session_id: session_id.to_string(),
            title: String::new(),
            directory: String::new(),
            project_id: None,
            parent_id: None,
            state: "idle".to_string(),
            last_assistant_message_id: None,
            last_notified_message_id: None,
            notification_pending: false,
            muted: false,
            updated_at: 0,
        };
        self.db.upsert_session(&session).await?;
        Ok(session)
    }
}
