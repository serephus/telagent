use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, StatusCode};
use reqwest_eventsource::{Event, EventSource};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    config::OpenCodeConfig,
    opencode::{
        backend::{AgentEvent, EventStream, OpenCodeBackend},
        types::{
            CacheTokens, MessageInfo, MessageTime, MessageTokens, MessageWithParts, Part,
            PermissionReply, PermissionRequest, QuestionRequest, RawEvent, SessionContext,
            SessionInfo, SessionModel, SessionStatus,
        },
    },
};

const HISTORY_PAGE_SIZE: usize = 200;
const MAX_HISTORY_PAGES: usize = 1000;

#[derive(Clone)]
pub struct V2Http {
    client: Client,
    base_url: String,
    auth: V2Auth,
    events_path: String,
}

#[derive(Debug, Clone)]
enum V2Auth {
    None,
    Basic { username: String, password: String },
    Bearer(String),
    Headers(Vec<(String, String)>),
}

impl V2Auth {
    fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Self::None => request,
            Self::Basic { username, password } => request.basic_auth(username, Some(password)),
            Self::Bearer(token) => request.bearer_auth(token),
            Self::Headers(headers) => {
                let mut request = request;
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                request
            }
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct ServiceFile {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    headers: Option<Value>,
}

impl ServiceFile {
    fn resolve_url(&self) -> Option<String> {
        if let Some(url) = &self.url
            && !url.trim().is_empty()
        {
            return Some(url.trim_end_matches('/').to_string());
        }
        let host = self.host.as_deref().or(self.hostname.as_deref())?;
        let port = self.port?;
        Some(format!("http://{host}:{port}"))
    }

    fn auth(&self) -> Option<V2Auth> {
        if let Some(headers) = &self.headers
            && let Some(headers) = headers.as_object()
        {
            let headers = headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|value| (name.clone(), value.to_string()))
                })
                .collect::<Vec<_>>();
            if !headers.is_empty() {
                return Some(V2Auth::Headers(headers));
            }
        }
        if let Some(token) = &self.token {
            return Some(V2Auth::Bearer(token.clone()));
        }
        if let Some(password) = &self.password {
            return Some(V2Auth::Basic {
                username: self
                    .username
                    .clone()
                    .unwrap_or_else(|| "opencode".to_string()),
                password: password.clone(),
            });
        }
        None
    }
}

fn discover_service(config: &OpenCodeConfig) -> Option<(String, Option<V2Auth>)> {
    let path = config
        .service_file
        .clone()
        .unwrap_or_else(default_service_file);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| {
            tracing::debug!(%error, path = %path.display(), "failed to read opencode service file");
            error
        })
        .ok()?;
    let service: ServiceFile = serde_json::from_str(&text)
        .map_err(|error| {
            tracing::warn!(%error, path = %path.display(), "failed to parse opencode service file");
            error
        })
        .ok()?;
    let url = service.resolve_url()?;
    Some((url, service.auth()))
}

fn default_service_file() -> std::path::PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("opencode")
        .join("service.json")
}

impl V2Http {
    pub fn new(config: &OpenCodeConfig) -> Result<Self> {
        let mut auth = match config.password_file.as_deref() {
            Some(path) => V2Auth::Basic {
                username: config.username.clone(),
                password: std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read {}", path.display()))?
                    .trim()
                    .to_string(),
            },
            None => V2Auth::None,
        };
        let mut base_url = config.url.trim_end_matches('/').to_string();

        if config.auto_discover {
            match discover_service(config) {
                Some((url, discovered_auth)) => {
                    base_url = url;
                    if let Some(discovered_auth) = discovered_auth {
                        auth = discovered_auth;
                    }
                }
                None => tracing::warn!(
                    "opencode service discovery failed; using configured URL {base_url}"
                ),
            }
        }

        let events_path = if config.events_path == "/global/event" {
            "/api/event".to_string()
        } else {
            config.events_path.clone()
        };

        Ok(Self {
            client: Client::builder()
                .build()
                .context("failed to create HTTP client")?,
            base_url,
            auth,
            events_path,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        self.auth.apply(self.client.request(method, url))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.request(reqwest::Method::GET, path).send().await?;
        decode(response).await
    }

    async fn post_json<B: serde::Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await?;
        ensure_success(response).await
    }

    async fn post_json_decode<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await?;
        decode(response).await
    }

    async fn post_empty(&self, path: &str) -> Result<()> {
        let response = self.request(reqwest::Method::POST, path).send().await?;
        ensure_success(response).await
    }
}

#[async_trait]
impl OpenCodeBackend for V2Http {
    fn name(&self) -> &'static str {
        "opencode-v2-http"
    }

    async fn health(&self) -> Result<()> {
        let response = self
            .request(reqwest::Method::GET, "/api/health")
            .send()
            .await?;
        ensure_success(response).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let response: SessionsResponse = self.get_json("/api/session?limit=50&order=desc").await?;
        Ok(response
            .data
            .into_iter()
            .map(session_info_from_v2_session)
            .collect())
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        let response = self
            .request(reqwest::Method::GET, &format!("/api/session/{session_id}"))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response: CreateSessionResponse = decode(response).await?;
        Ok(Some(session_info_from_v2_session(response.data)))
    }

    async fn create_session(&self, directory: &str) -> Result<SessionInfo> {
        let directory = if directory.is_empty() {
            std::env::current_dir()
                .context("failed to resolve current directory")?
                .to_string_lossy()
                .into_owned()
        } else {
            directory.to_string()
        };
        let response: CreateSessionResponse = self
            .post_json_decode(
                "/api/session",
                &json!({"location": {"directory": directory}}),
            )
            .await?;
        Ok(session_info_from_v2_session(response.data))
    }

    async fn compact_session(&self, session_id: &str, _directory: &str) -> Result<()> {
        self.post_empty(&format!("/api/session/{session_id}/compact"))
            .await
    }

    async fn session_context(
        &self,
        session_id: &str,
        _directory: &str,
    ) -> Result<Option<SessionContext>> {
        let Some(session) = self.get_session(session_id).await? else {
            return Ok(None);
        };

        let messages = self.messages(session_id, "", 10).await?;
        let used_tokens = messages
            .iter()
            .filter(|message| message.is_assistant())
            .max_by_key(|message| message.info.time.created)
            .map(|message| message.info.tokens.total())
            .filter(|tokens| *tokens > 0);

        let model = session.model;
        let context_limit = match &model {
            Some(model) => {
                let response: Value = self.get_json("/api/model").await?;
                response
                    .get("data")
                    .and_then(Value::as_array)
                    .and_then(|models| {
                        models
                            .iter()
                            .find(|candidate| {
                                candidate.get("id").and_then(Value::as_str)
                                    == Some(model.id.as_str())
                                    && candidate.get("providerID").and_then(Value::as_str)
                                        == Some(model.provider_id.as_str())
                            })
                            .and_then(|candidate| candidate.get("limit"))
                            .and_then(|limit| limit.get("context"))
                            .and_then(Value::as_u64)
                    })
            }
            None => None,
        };

        Ok(Some(SessionContext {
            model: model.map(|model| model.id),
            used_tokens,
            context_limit,
        }))
    }

    async fn session_statuses(&self) -> Result<HashMap<String, SessionStatus>> {
        let response: ActiveResponse = self.get_json("/api/session/active").await?;
        Ok(response
            .data
            .into_keys()
            .map(|session_id| (session_id, SessionStatus::Busy))
            .collect())
    }

    async fn messages(
        &self,
        session_id: &str,
        _directory: &str,
        limit: usize,
    ) -> Result<Vec<MessageWithParts>> {
        let response: MessagesResponse = self
            .get_json(&format!(
                "/api/session/{session_id}/message?limit={limit}&order=desc"
            ))
            .await?;
        Ok(response.data.iter().filter_map(normalize_message).collect())
    }

    async fn history(&self, session_id: &str, _directory: &str) -> Result<Vec<MessageWithParts>> {
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_HISTORY_PAGES {
            let path = match &cursor {
                Some(cursor) => format!(
                    "/api/session/{session_id}/message?limit={HISTORY_PAGE_SIZE}&cursor={cursor}"
                ),
                None => {
                    format!("/api/session/{session_id}/message?limit={HISTORY_PAGE_SIZE}&order=asc")
                }
            };
            let response: MessagesResponse = self.get_json(&path).await?;
            messages.extend(response.data.iter().filter_map(normalize_message));
            match response.cursor.next {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => return Ok(messages),
            }
        }
        bail!("session {session_id} history exceeds {MAX_HISTORY_PAGES} pages")
    }

    async fn prompt_async(&self, session_id: &str, _directory: &str, text: &str) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            prompt: Prompt<'a>,
        }

        #[derive(serde::Serialize)]
        struct Prompt<'a> {
            text: &'a str,
        }

        self.post_json(
            &format!("/api/session/{session_id}/prompt"),
            &Body {
                prompt: Prompt { text },
            },
        )
        .await
    }

    async fn abort(&self, session_id: &str, _directory: &str) -> Result<()> {
        self.post_empty(&format!("/api/session/{session_id}/interrupt"))
            .await
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        _directory: &str,
        request_id: &str,
        reply: PermissionReply,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            reply: &'a str,
        }
        self.post_json(
            &format!("/api/session/{session_id}/permission/{request_id}/reply"),
            &Body {
                reply: reply.as_str(),
            },
        )
        .await
    }

    async fn pending_permission_ids(
        &self,
        session_id: &str,
        _directory: &str,
    ) -> Result<Option<HashSet<String>>> {
        let response: Value = self
            .get_json(&format!("/api/session/{session_id}/permission"))
            .await?;
        let ids = response
            .get("data")
            .and_then(Value::as_array)
            .map(|permissions| {
                permissions
                    .iter()
                    .filter_map(|permission| {
                        permission
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(ids))
    }

    async fn pending_question_ids(
        &self,
        session_id: &str,
        _directory: &str,
    ) -> Result<Option<HashSet<String>>> {
        let response: Value = self
            .get_json(&format!("/api/session/{session_id}/question"))
            .await?;
        let ids = response
            .get("data")
            .and_then(Value::as_array)
            .map(|questions| {
                questions
                    .iter()
                    .filter_map(|question| {
                        question
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(ids))
    }

    async fn reply_question(
        &self,
        session_id: &str,
        _directory: &str,
        request_id: &str,
        request: &QuestionRequest,
        answers: Vec<Vec<String>>,
    ) -> Result<()> {
        let answers = answers
            .into_iter()
            .enumerate()
            .map(|(index, values)| {
                let question = request.questions.get(index);
                values
                    .into_iter()
                    .map(|value| {
                        question
                            .and_then(|question| {
                                question
                                    .options
                                    .iter()
                                    .find(|option| option.value.as_deref() == Some(value.as_str()))
                            })
                            .map(|option| option.label.clone())
                            .unwrap_or(value)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        self.post_json(
            &format!("/api/session/{session_id}/question/{request_id}/reply"),
            &json!({ "answers": answers }),
        )
        .await
    }

    async fn event_stream(&self) -> Result<EventStream> {
        let request = self.request(reqwest::Method::GET, &self.events_path);
        let mut source = EventSource::new(request).context("failed to create SSE request")?;

        let stream = async_stream::try_stream! {
            while let Some(event) = source.next().await {
                match event {
                    Ok(Event::Open) => {
                        tracing::debug!("opencode v2 event stream opened");
                    }
                    Ok(Event::Message(message)) => {
                        if message.data.trim().is_empty() {
                            continue;
                        }
                        let value: Value = serde_json::from_str(&message.data)
                            .with_context(|| format!("invalid opencode v2 event: {}", message.data))?;
                        let event_type = value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let properties = value.get("data").cloned().unwrap_or(Value::Null);
                        yield RawEvent {
                            event_type,
                            properties,
                        };
                    }
                    Err(error) => {
                        Err(anyhow::anyhow!("opencode v2 event stream error: {error}"))?;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn normalize_event(&self, raw: RawEvent) -> AgentEvent {
        normalize_v2_event(raw)
    }
}

#[derive(Debug, Deserialize)]
struct SessionsResponse {
    data: Vec<V2Session>,
}

#[derive(Debug, Deserialize)]
struct CreateSessionResponse {
    data: V2Session,
}

#[derive(Debug, Deserialize)]
struct V2Session {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "projectID", default)]
    project_id: Option<String>,
    #[serde(rename = "parentID", default)]
    parent_id: Option<String>,
    #[serde(default)]
    location: Option<V2Location>,
    #[serde(default)]
    model: Option<V2Model>,
}

#[derive(Debug, Deserialize)]
struct V2Model {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
}

#[derive(Debug, Deserialize)]
struct V2Location {
    directory: String,
}

fn session_info_from_v2_session(session: V2Session) -> SessionInfo {
    SessionInfo {
        id: session.id,
        title: session.title.unwrap_or_default(),
        directory: session
            .location
            .map(|location| location.directory)
            .unwrap_or_default(),
        project_id: session.project_id,
        parent_id: session.parent_id,
        model: session
            .model
            .map(|model| crate::opencode::types::SessionModel {
                id: model.id,
                provider_id: model.provider_id,
            }),
    }
}

#[derive(Debug, Deserialize)]
struct ActiveResponse {
    data: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    data: Vec<Value>,
    #[serde(default)]
    cursor: MessagesCursor,
}

#[derive(Debug, Default, Deserialize)]
struct MessagesCursor {
    #[serde(default)]
    next: Option<String>,
}

fn normalize_message(value: &Value) -> Option<MessageWithParts> {
    let role = value.get("type")?.as_str()?;
    if role != "assistant" && role != "user" {
        return None;
    }
    let id = value.get("id")?.as_str()?.to_string();
    let created = value
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(number_to_i64)
        .unwrap_or_default();
    let completed = value
        .get("time")
        .and_then(|time| time.get("completed"))
        .and_then(number_to_i64);

    let parts = if let Some(text) = value.get("text").and_then(Value::as_str) {
        vec![Part {
            kind: "text".to_string(),
            text: Some(text.to_string()),
            ..Default::default()
        }]
    } else {
        value
            .get("content")
            .and_then(Value::as_array)
            .map(|content| content.iter().filter_map(part_from_value).collect())
            .unwrap_or_default()
    };

    Some(MessageWithParts {
        info: MessageInfo {
            id,
            role: role.to_string(),
            time: MessageTime { created, completed },
            tokens: tokens_from_v2(value),
        },
        parts,
    })
}

fn part_from_value(value: &Value) -> Option<Part> {
    match serde_json::from_value::<Part>(value.clone()) {
        Ok(part) => Some(part),
        Err(_) => {
            let kind = value.get("type").and_then(Value::as_str)?.to_string();
            let mut extra = match value {
                Value::Object(map) => map.clone(),
                _ => return None,
            };
            extra.remove("type");
            Some(Part {
                kind,
                extra,
                ..Default::default()
            })
        }
    }
}

fn tokens_from_v2(value: &Value) -> MessageTokens {
    let tokens = value.get("tokens");
    let cache = tokens.and_then(|tokens| tokens.get("cache"));
    MessageTokens {
        total: 0,
        input: tokens
            .and_then(|tokens| tokens.get("input"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: tokens
            .and_then(|tokens| tokens.get("output"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning: tokens
            .and_then(|tokens| tokens.get("reasoning"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache: CacheTokens {
            read: cache
                .and_then(|cache| cache.get("read"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            write: cache
                .and_then(|cache| cache.get("write"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
    }
}

fn number_to_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|value| value as i64))
}

pub fn normalize_v2_event(raw: RawEvent) -> AgentEvent {
    let data = &raw.properties;
    match raw.event_type.as_str() {
        "session.created" | "session.updated" => session_info_from_v2(data)
            .map(|info| {
                if raw.event_type == "session.created" {
                    AgentEvent::SessionCreated(info)
                } else {
                    AgentEvent::SessionUpdated(info)
                }
            })
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.deleted" => data
            .get("sessionID")
            .and_then(Value::as_str)
            .map(|session_id| AgentEvent::SessionDeleted(session_id.to_string()))
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.status" => {
            let session_id = data.get("sessionID").and_then(Value::as_str);
            let status = data
                .get("status")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            match (session_id, status) {
                (Some(session_id), Some(status)) => AgentEvent::SessionStatus {
                    session_id: session_id.to_string(),
                    status,
                },
                _ => AgentEvent::Other(raw.event_type),
            }
        }
        "session.execution.started" => data
            .get("sessionID")
            .and_then(Value::as_str)
            .map(|session_id| AgentEvent::SessionStatus {
                session_id: session_id.to_string(),
                status: SessionStatus::Busy,
            })
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.execution.succeeded" | "session.idle" => data
            .get("sessionID")
            .and_then(Value::as_str)
            .map(|session_id| AgentEvent::SessionIdle(session_id.to_string()))
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.execution.interrupted" => data
            .get("sessionID")
            .and_then(Value::as_str)
            .map(|session_id| AgentEvent::SessionStatus {
                session_id: session_id.to_string(),
                status: SessionStatus::Idle,
            })
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "session.error" | "session.execution.failed" => {
            let session_id = data.get("sessionID").and_then(Value::as_str);
            let message = data
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            session_id
                .map(|session_id| AgentEvent::SessionError {
                    session_id: session_id.to_string(),
                    message,
                })
                .unwrap_or(AgentEvent::Other(raw.event_type))
        }
        "permission.asked" | "permission.v2.asked" => permission_request_from_v2(data)
            .map(AgentEvent::PermissionAsked)
            .unwrap_or(AgentEvent::Other(raw.event_type)),
        "permission.replied" | "permission.v2.replied" => AgentEvent::PermissionReplied {
            session_id: data
                .get("sessionID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            request_id: data
                .get("requestID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            reply: data
                .get("reply")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "question.asked" | "question.v2.asked" => {
            serde_json::from_value::<QuestionRequest>(data.clone())
                .map(AgentEvent::QuestionAsked)
                .unwrap_or(AgentEvent::Other(raw.event_type))
        }
        "question.replied"
        | "question.rejected"
        | "question.v2.replied"
        | "question.v2.rejected" => {
            let request_id = data
                .get("requestID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let session_id = data
                .get("sessionID")
                .and_then(Value::as_str)
                .map(str::to_string);
            AgentEvent::QuestionReplied {
                session_id,
                request_id,
            }
        }
        other => AgentEvent::Other(other.to_string()),
    }
}

fn session_info_from_v2(data: &Value) -> Option<SessionInfo> {
    let info = data.get("info").unwrap_or(data);
    let id = info
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| data.get("sessionID").and_then(Value::as_str))?;
    Some(SessionInfo {
        id: id.to_string(),
        title: info
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        directory: info
            .get("directory")
            .and_then(Value::as_str)
            .or_else(|| {
                info.get("location")
                    .and_then(|location| location.get("directory"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string(),
        project_id: info
            .get("projectID")
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_id: info
            .get("parentID")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: info.get("model").and_then(session_model_from_v2),
    })
}

fn session_model_from_v2(model: &Value) -> Option<SessionModel> {
    Some(SessionModel {
        id: model.get("id")?.as_str()?.to_string(),
        provider_id: model.get("providerID")?.as_str()?.to_string(),
    })
}

fn permission_request_from_v2(data: &Value) -> Option<PermissionRequest> {
    serde_json::from_value(data.clone())
        .ok()
        .or_else(|| permission_from_v2(data))
}

fn permission_from_v2(data: &Value) -> Option<PermissionRequest> {
    Some(PermissionRequest {
        id: data.get("id")?.as_str()?.to_string(),
        session_id: data.get("sessionID")?.as_str()?.to_string(),
        permission: data
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("permission")
            .to_string(),
        patterns: string_array(data.get("resources")),
        metadata: data.get("metadata").cloned().unwrap_or(Value::Null),
        always: string_array(data.get("save")),
    })
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("opencode v2 request failed with {status}: {body}");
    }
    response.json::<T>().await.map_err(Into::into)
}

async fn ensure_success(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() || status == StatusCode::NO_CONTENT {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    bail!("opencode v2 request failed with {status}: {body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_v2_permission_event() {
        let raw = RawEvent {
            event_type: "permission.asked".to_string(),
            properties: json!({
                "id": "per_1",
                "sessionID": "ses_1",
                "action": "bash",
                "resources": ["rm -rf /tmp/x"],
                "save": ["bash"]
            }),
        };
        match normalize_v2_event(raw) {
            AgentEvent::PermissionAsked(request) => {
                assert_eq!(request.id, "per_1");
                assert_eq!(request.session_id, "ses_1");
                assert_eq!(request.permission, "bash");
                assert_eq!(request.patterns, vec!["rm -rf /tmp/x"]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_v2_question_event() {
        let raw = RawEvent {
            event_type: "question.asked".to_string(),
            properties: json!({
                "id": "que_1",
                "sessionID": "ses_1",
                "questions": [
                    {
                        "question": "Which mode?",
                        "header": "mode",
                        "options": [
                            {"label": "Fast", "description": "Fast mode"},
                            {"label": "Safe", "description": "Safe mode"}
                        ]
                    }
                ]
            }),
        };
        match normalize_v2_event(raw) {
            AgentEvent::QuestionAsked(request) => {
                assert_eq!(request.id, "que_1");
                assert_eq!(request.questions[0].header, "mode");
                assert_eq!(request.questions[0].options[0].label, "Fast");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_v2_v1_permission_event() {
        let raw = RawEvent {
            event_type: "permission.asked".to_string(),
            properties: json!({
                "id": "per_1",
                "sessionID": "ses_1",
                "permission": "read",
                "patterns": ["/tmp/.env"],
                "metadata": {},
                "always": ["/tmp/.env"]
            }),
        };
        match normalize_v2_event(raw) {
            AgentEvent::PermissionAsked(request) => {
                assert_eq!(request.id, "per_1");
                assert_eq!(request.permission, "read");
                assert_eq!(request.patterns, vec!["/tmp/.env"]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn normalizes_v2_session_event() {
        let raw = RawEvent {
            event_type: "session.updated".to_string(),
            properties: json!({
                "sessionID": "ses_1",
                "info": {
                    "id": "ses_1",
                    "title": "Test session",
                    "directory": "/tmp/project",
                    "projectID": "global",
                    "model": {"id": "gpt-5", "providerID": "openai"}
                }
            }),
        };
        match normalize_v2_event(raw) {
            AgentEvent::SessionUpdated(session) => {
                assert_eq!(session.id, "ses_1");
                assert_eq!(session.title, "Test session");
                assert_eq!(session.directory, "/tmp/project");
                assert_eq!(session.model.unwrap().id, "gpt-5");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn normalizes_v2_assistant_message() {
        let message = normalize_message(&json!({
            "id": "msg_1",
            "type": "assistant",
            "time": {"created": 10, "completed": 20},
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "reasoning", "text": "ignored"}
            ]
        }))
        .unwrap();

        assert!(message.is_assistant());
        assert!(message.is_completed());
        assert_eq!(message.text(), "hello");
        assert_eq!(message.parts.len(), 2);
        assert_eq!(message.parts[1].kind, "reasoning");
        assert_eq!(message.parts[1].text.as_deref(), Some("ignored"));
    }

    #[test]
    fn normalizes_v2_tool_part_fields() {
        let message = normalize_message(&json!({
            "id": "msg_1",
            "type": "assistant",
            "content": [{
                "type": "tool",
                "tool": "bash",
                "callID": "call_1",
                "state": {
                    "status": "completed",
                    "input": {"command": "ls"},
                    "output": "file.txt"
                }
            }]
        }))
        .unwrap();

        let part = &message.parts[0];
        assert_eq!(part.kind, "tool");
        assert_eq!(part.tool.as_deref(), Some("bash"));
        assert_eq!(part.call_id.as_deref(), Some("call_1"));
        let state = part.state.as_ref().unwrap();
        assert_eq!(state.status, "completed");
        assert_eq!(state.output.as_deref(), Some("file.txt"));
    }

    #[test]
    fn parses_service_file_url_and_auth() {
        let service: ServiceFile = serde_json::from_value(json!({
            "url": "http://127.0.0.1:4321/",
            "password": "secret"
        }))
        .unwrap();

        assert_eq!(
            service.resolve_url().as_deref(),
            Some("http://127.0.0.1:4321")
        );
        match service.auth() {
            Some(V2Auth::Basic { username, password }) => {
                assert_eq!(username, "opencode");
                assert_eq!(password, "secret");
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }

    #[test]
    fn parses_service_file_host_port_and_headers() {
        let service: ServiceFile = serde_json::from_value(json!({
            "hostname": "127.0.0.1",
            "port": 4322,
            "headers": {"Authorization": "Bearer test"}
        }))
        .unwrap();

        assert_eq!(
            service.resolve_url().as_deref(),
            Some("http://127.0.0.1:4322")
        );
        match service.auth() {
            Some(V2Auth::Headers(headers)) => {
                assert_eq!(
                    headers,
                    vec![("Authorization".to_string(), "Bearer test".to_string())]
                );
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        extract::{Path, Query},
        http::{StatusCode as AxumStatus, header},
        routing::{get, post},
    };
    use futures_util::StreamExt;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        config::OpenCodeConfig,
        opencode::types::{QuestionInfo, QuestionOption},
    };

    type Captured = Arc<Mutex<Option<Value>>>;

    fn app() -> (Router, Captured, Captured, Captured) {
        let prompt_captured: Captured = Arc::new(Mutex::new(None));
        let form_captured: Captured = Arc::new(Mutex::new(None));
        let permission_captured: Captured = Arc::new(Mutex::new(None));

        let prompt_state = prompt_captured.clone();
        let prompt = post(move |Json(body): Json<Value>| {
            let captured = prompt_state.clone();
            async move {
                *captured.lock().unwrap() = Some(body);
                AxumStatus::OK
            }
        });

        let form_state = form_captured.clone();
        let form = post(
            move |Path((_session_id, _form_id)): Path<(String, String)>,
                  Json(body): Json<Value>| {
                let captured = form_state.clone();
                async move {
                    *captured.lock().unwrap() = Some(body);
                    AxumStatus::NO_CONTENT
                }
            },
        );

        let permission_state = permission_captured.clone();
        let permission = post(
            move |Path((_session_id, _request_id)): Path<(String, String)>,
                  Json(body): Json<Value>| {
                let captured = permission_state.clone();
                async move {
                    *captured.lock().unwrap() = Some(body);
                    AxumStatus::NO_CONTENT
                }
            },
        );

        let router = Router::new()
            .route(
                "/api/event",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "data: {\"type\":\"session.execution.succeeded\",\"data\":{\"sessionID\":\"ses_1\"}}\n\n",
                    )
                }),
            )
            .route(
                "/api/session",
                get(|| async {
                    Json(json!({
                        "data": [{
                            "id": "ses_1",
                            "title": "Test session",
                            "projectID": "prj_1",
                            "location": {"directory": "/tmp"}
                        }],
                        "cursor": {"previous": null, "next": null}
                    }))
                })
                .post(|| async {
                    Json(json!({
                        "data": {
                            "id": "ses_2",
                            "title": "New session",
                            "projectID": "prj_1",
                            "location": {"directory": "/tmp/project"}
                        }
                    }))
                }),
            )
            .route(
                "/api/health",
                get(|| async { Json(json!({"healthy": true, "version": "test", "pid": 1})) }),
            )
            .route(
                "/api/session/active",
                get(|| async { Json(json!({"data": {"ses_1": {"type": "running"}}})) }),
            )
            .route(
                "/api/session/{session_id}/message",
                get(|| async {
                    Json(json!({
                        "data": [{
                            "id": "msg_1",
                            "type": "assistant",
                            "time": {"created": 1, "completed": 2},
                            "content": [{"type": "text", "text": "done"}]
                        }],
                        "cursor": {"previous": null, "next": null}
                    }))
                }),
            )
            .route("/api/session/{session_id}/prompt", prompt)
            .route(
                "/api/session/{session_id}/interrupt",
                post(|| async { AxumStatus::OK }),
            )
            .route(
                "/api/session/{session_id}/compact",
                post(|| async { AxumStatus::NO_CONTENT }),
            )
            .route(
                "/api/session/{session_id}/permission/{request_id}/reply",
                permission,
            )
            .route(
                "/api/session/{session_id}/question/{request_id}/reply",
                form,
            );

        (router, prompt_captured, form_captured, permission_captured)
    }

    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn fetches_all_history_pages_in_order() {
        use std::collections::HashMap;

        let router = Router::new().route(
            "/api/session/{session_id}/message",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                if query.get("cursor").map(String::as_str) == Some("next") {
                    Json(json!({
                        "data": [{
                            "id": "msg_2",
                            "type": "assistant",
                            "time": {"created": 2, "completed": 3},
                            "content": [{"type": "text", "text": "hi"}]
                        }],
                        "cursor": {"previous": "previous", "next": null}
                    }))
                } else {
                    Json(json!({
                        "data": [{
                            "id": "msg_1",
                            "type": "user",
                            "time": {"created": 1},
                            "text": "hello"
                        }],
                        "cursor": {"previous": null, "next": "next"}
                    }))
                }
            }),
        );
        let url = spawn(router).await;
        let client = V2Http::new(&OpenCodeConfig {
            backend: "v2".to_string(),
            url,
            ..Default::default()
        })
        .unwrap();

        let messages = client.history("ses_1", "").await.unwrap();
        assert_eq!(messages.len(), 2);
        assert!(!messages[0].is_assistant());
        assert_eq!(messages[0].text(), "hello");
        assert!(messages[1].is_assistant());
        assert_eq!(messages[1].text(), "hi");
    }

    #[tokio::test]
    async fn talks_to_v2_api() {
        let (router, prompt_captured, form_captured, permission_captured) = app();
        let url = spawn(router).await;
        let config = OpenCodeConfig {
            backend: "v2".to_string(),
            url,
            ..Default::default()
        };
        let client = V2Http::new(&config).unwrap();

        client.health().await.unwrap();
        let sessions = client.list_sessions().await.unwrap();
        assert_eq!(sessions[0].id, "ses_1");
        assert_eq!(sessions[0].title, "Test session");
        assert_eq!(sessions[0].directory, "/tmp");

        let created = client.create_session("/tmp/project").await.unwrap();
        assert_eq!(created.id, "ses_2");
        assert_eq!(created.directory, "/tmp/project");

        let statuses = client.session_statuses().await.unwrap();
        assert_eq!(statuses.get("ses_1"), Some(&SessionStatus::Busy));

        let messages = client.messages("ses_1", "", 10).await.unwrap();
        assert_eq!(messages[0].text(), "done");

        client.prompt_async("ses_1", "", "hello").await.unwrap();
        assert_eq!(
            prompt_captured.lock().unwrap().as_ref().unwrap()["prompt"]["text"],
            "hello"
        );

        client.abort("ses_1", "").await.unwrap();
        client.compact_session("ses_1", "").await.unwrap();

        client
            .reply_permission("ses_1", "", "per_1", PermissionReply::Once)
            .await
            .unwrap();
        assert_eq!(
            permission_captured.lock().unwrap().as_ref().unwrap()["reply"],
            "once"
        );

        let request = QuestionRequest {
            id: "que_1".to_string(),
            session_id: "ses_1".to_string(),
            questions: vec![QuestionInfo {
                question: "Mode".to_string(),
                header: "mode".to_string(),
                options: vec![QuestionOption {
                    label: "Fast".to_string(),
                    description: None,
                    value: Some("fast".to_string()),
                }],
                multiple: false,
                custom: false,
                key: Some("mode".to_string()),
                input_type: Some("string".to_string()),
            }],
        };
        client
            .reply_question(
                "ses_1",
                "",
                "que_1",
                &request,
                vec![vec!["fast".to_string()]],
            )
            .await
            .unwrap();
        assert_eq!(
            form_captured.lock().unwrap().as_ref().unwrap()["answers"][0][0],
            "Fast"
        );

        let mut stream = client.event_stream().await.unwrap();
        let raw = stream.next().await.unwrap().unwrap();
        assert_eq!(raw.event_type, "session.execution.succeeded");
        assert!(matches!(
            normalize_v2_event(raw),
            AgentEvent::SessionIdle(id) if id == "ses_1"
        ));
    }
}
