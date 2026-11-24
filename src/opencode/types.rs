use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub directory: String,
    #[serde(rename = "projectID", default)]
    pub project_id: Option<String>,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionStatus {
    Idle,
    Busy,
    Retry {
        #[serde(default)]
        attempt: u32,
        #[serde(default)]
        message: String,
    },
}

impl SessionStatus {
    pub fn as_db_state(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Retry { .. } => "retry",
        }
    }

    pub fn retry_message(&self) -> Option<&str> {
        match self {
            Self::Retry { message, .. } if !message.is_empty() => Some(message),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageWithParts {
    pub info: MessageInfo,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageInfo {
    pub id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub time: MessageTime,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MessageTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub completed: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Part {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

impl MessageWithParts {
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter(|part| part.kind == "text")
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn is_assistant(&self) -> bool {
        self.info.role == "assistant"
    }

    pub fn is_completed(&self) -> bool {
        self.info.time.completed.is_some()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub permission: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub always: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub questions: Vec<QuestionInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuestionInfo {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub custom: bool,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub input_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub properties: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

impl PermissionReply {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}

pub fn session_event_info(value: &serde_json::Value) -> Option<SessionInfo> {
    value
        .get("info")
        .cloned()
        .and_then(|info| serde_json::from_value(info).ok())
}

pub fn session_event_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("sessionID")
        .or_else(|| value.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}
