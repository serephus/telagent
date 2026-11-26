use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
};

use async_trait::async_trait;
use futures_util::Stream;

use crate::opencode::types::{
    MessageWithParts, PermissionReply, PermissionRequest, QuestionRequest, RawEvent,
    SessionContext, SessionInfo, SessionStatus,
};

pub type EventStream = Pin<Box<dyn Stream<Item = anyhow::Result<RawEvent>> + Send>>;

#[async_trait]
pub trait OpenCodeBackend: Send + Sync {
    fn name(&self) -> &'static str;

    async fn health(&self) -> anyhow::Result<()>;

    async fn list_sessions(&self) -> anyhow::Result<Vec<SessionInfo>>;

    async fn get_session(&self, session_id: &str) -> anyhow::Result<Option<SessionInfo>>;

    async fn create_session(&self, directory: &str) -> anyhow::Result<SessionInfo>;

    async fn compact_session(&self, session_id: &str, directory: &str) -> anyhow::Result<()>;

    async fn session_context(
        &self,
        session_id: &str,
        directory: &str,
    ) -> anyhow::Result<Option<SessionContext>>;

    async fn session_statuses(&self) -> anyhow::Result<HashMap<String, SessionStatus>>;

    async fn messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageWithParts>>;

    async fn history(
        &self,
        session_id: &str,
        directory: &str,
    ) -> anyhow::Result<Vec<MessageWithParts>>;

    async fn prompt_async(
        &self,
        session_id: &str,
        directory: &str,
        text: &str,
    ) -> anyhow::Result<()>;

    async fn abort(&self, session_id: &str, directory: &str) -> anyhow::Result<()>;

    async fn reply_permission(
        &self,
        session_id: &str,
        directory: &str,
        request_id: &str,
        reply: PermissionReply,
    ) -> anyhow::Result<()>;

    async fn pending_permission_ids(
        &self,
        session_id: &str,
        directory: &str,
    ) -> anyhow::Result<Option<HashSet<String>>>;

    async fn pending_question_ids(
        &self,
        session_id: &str,
        directory: &str,
    ) -> anyhow::Result<Option<HashSet<String>>>;

    async fn reply_question(
        &self,
        session_id: &str,
        directory: &str,
        request_id: &str,
        request: &QuestionRequest,
        answers: Vec<Vec<String>>,
    ) -> anyhow::Result<()>;

    async fn event_stream(&self) -> anyhow::Result<EventStream>;

    fn normalize_event(&self, raw: RawEvent) -> AgentEvent;
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    SessionCreated(SessionInfo),
    SessionUpdated(SessionInfo),
    SessionDeleted(String),
    SessionStatus {
        session_id: String,
        status: SessionStatus,
    },
    SessionIdle(String),
    SessionError {
        session_id: String,
        message: String,
    },
    PermissionAsked(PermissionRequest),
    PermissionReplied {
        session_id: String,
        request_id: String,
        reply: String,
    },
    QuestionAsked(QuestionRequest),
    QuestionReplied {
        session_id: Option<String>,
        request_id: String,
    },
    Other(String),
}

impl AgentEvent {
    pub fn kind(&self) -> &str {
        match self {
            Self::SessionCreated(_) => "session.created",
            Self::SessionUpdated(_) => "session.updated",
            Self::SessionDeleted(_) => "session.deleted",
            Self::SessionStatus { .. } => "session.status",
            Self::SessionIdle(_) => "session.idle",
            Self::SessionError { .. } => "session.error",
            Self::PermissionAsked(_) => "permission.asked",
            Self::PermissionReplied { .. } => "permission.replied",
            Self::QuestionAsked(_) => "question.asked",
            Self::QuestionReplied { .. } => "question.replied",
            Self::Other(kind) => kind,
        }
    }
}

pub fn normalize_v1_event(raw: RawEvent) -> AgentEvent {
    let props = &raw.properties;
    match raw.event_type.as_str() {
        "session.created" | "session.updated" => {
            if let Some(info) = crate::opencode::types::session_event_info(props) {
                if raw.event_type == "session.created" {
                    AgentEvent::SessionCreated(info)
                } else {
                    AgentEvent::SessionUpdated(info)
                }
            } else {
                AgentEvent::Other(raw.event_type)
            }
        }
        "session.deleted" => crate::opencode::types::session_event_id(props)
            .map(AgentEvent::SessionDeleted)
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.status" => {
            let session_id = crate::opencode::types::session_event_id(props);
            let status = props
                .get("status")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            match (session_id, status) {
                (Some(session_id), Some(status)) => {
                    AgentEvent::SessionStatus { session_id, status }
                }
                _ => AgentEvent::Other(raw.event_type),
            }
        }
        "session.idle" => crate::opencode::types::session_event_id(props)
            .map(AgentEvent::SessionIdle)
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.error" => {
            let session_id = crate::opencode::types::session_event_id(props);
            let message = props
                .get("error")
                .map(error_message)
                .unwrap_or_else(|| "unknown error".to_string());
            session_id
                .map(|session_id| AgentEvent::SessionError {
                    session_id,
                    message,
                })
                .unwrap_or(AgentEvent::Other(raw.event_type))
        }
        "permission.asked" => serde_json::from_value(props.clone())
            .map(AgentEvent::PermissionAsked)
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "permission.replied" => {
            let session_id = crate::opencode::types::session_event_id(props).unwrap_or_default();
            let request_id = props
                .get("requestID")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let reply = props
                .get("reply")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            AgentEvent::PermissionReplied {
                session_id,
                request_id,
                reply,
            }
        }
        "question.asked" => serde_json::from_value(props.clone())
            .map(AgentEvent::QuestionAsked)
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "question.replied" | "question.rejected" => {
            let request_id = props
                .get("requestID")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let session_id = crate::opencode::types::session_event_id(props);
            AgentEvent::QuestionReplied {
                session_id,
                request_id,
            }
        }
        other => AgentEvent::Other(other.to_string()),
    }
}

fn error_message(value: &serde_json::Value) -> String {
    if let Some(message) = value.get("message").and_then(|v| v.as_str()) {
        return message.to_string();
    }
    if let Some(message) = value
        .get("data")
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
    {
        return message.to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(value: &str) -> RawEvent {
        serde_json::from_str(value).unwrap()
    }

    #[test]
    fn normalizes_idle_event() {
        let event = normalize_v1_event(raw(
            r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#,
        ));
        assert!(matches!(event, AgentEvent::SessionIdle(id) if id == "ses_1"));
    }

    #[test]
    fn normalizes_status_event() {
        let event = normalize_v1_event(raw(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"busy"}}}"#,
        ));
        match event {
            AgentEvent::SessionStatus { session_id, status } => {
                assert_eq!(session_id, "ses_1");
                assert_eq!(status, SessionStatus::Busy);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
