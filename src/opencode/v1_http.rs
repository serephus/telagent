use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, StatusCode};
use reqwest_eventsource::{Event, EventSource};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    config::OpenCodeConfig,
    opencode::{
        backend::{EventStream, OpenCodeBackend},
        types::{MessageWithParts, PermissionReply, RawEvent, SessionInfo, SessionStatus},
    },
};

#[derive(Clone)]
pub struct V1Http {
    client: Client,
    base_url: String,
    username: String,
    password: Option<String>,
    events_path: String,
}

impl V1Http {
    pub fn new(config: &OpenCodeConfig) -> Result<Self> {
        let password = match config.password_file.as_deref() {
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
                .context("failed to create HTTP client")?,
            base_url: config.url.trim_end_matches('/').to_string(),
            username: config.username.clone(),
            password,
            events_path: config.events_path.clone(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut request = self.client.request(method, url);
        if let Some(password) = &self.password {
            request = request.basic_auth(&self.username, Some(password));
        }
        request
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str, directory: &str) -> Result<T> {
        let response = self
            .request(reqwest::Method::GET, path)
            .query(&directory_query(directory))
            .send()
            .await?;
        decode(response).await
    }

    async fn get_json_optional<T: DeserializeOwned>(
        &self,
        path: &str,
        directory: &str,
    ) -> Result<Option<T>> {
        let response = self
            .request(reqwest::Method::GET, path)
            .query(&directory_query(directory))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        decode(response).await.map(Some)
    }

    async fn post_json<B: Serialize>(&self, path: &str, directory: &str, body: &B) -> Result<()> {
        let response = self
            .request(reqwest::Method::POST, path)
            .query(&directory_query(directory))
            .json(body)
            .send()
            .await?;
        ensure_success(response).await
    }

    async fn post_json_decode<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        directory: &str,
        body: &B,
    ) -> Result<T> {
        let response = self
            .request(reqwest::Method::POST, path)
            .query(&directory_query(directory))
            .json(body)
            .send()
            .await?;
        decode(response).await
    }

    async fn post_empty(&self, path: &str, directory: &str) -> Result<()> {
        let response = self
            .request(reqwest::Method::POST, path)
            .query(&directory_query(directory))
            .send()
            .await?;
        ensure_success(response).await
    }

    fn event_request(&self) -> RequestBuilder {
        self.request(reqwest::Method::GET, &self.events_path)
    }
}

#[async_trait]
impl OpenCodeBackend for V1Http {
    fn name(&self) -> &'static str {
        "opencode-v1-http"
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        self.get_json("/session", "").await
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        self.get_json_optional(&format!("/session/{session_id}"), "")
            .await
    }

    async fn create_session(&self, directory: &str) -> Result<SessionInfo> {
        #[derive(Serialize)]
        struct Body {}

        self.post_json_decode("/session", directory, &Body {}).await
    }

    async fn session_statuses(&self) -> Result<HashMap<String, SessionStatus>> {
        self.get_json("/session/status", "").await
    }

    async fn messages(
        &self,
        session_id: &str,
        directory: &str,
        limit: usize,
    ) -> Result<Vec<MessageWithParts>> {
        self.get_json(
            &format!("/session/{session_id}/message?limit={limit}"),
            directory,
        )
        .await
    }

    async fn prompt_async(&self, session_id: &str, directory: &str, text: &str) -> Result<()> {
        #[derive(Serialize)]
        struct TextPart<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            text: &'a str,
        }

        #[derive(Serialize)]
        struct Body<'a> {
            parts: Vec<TextPart<'a>>,
        }

        let body = Body {
            parts: vec![TextPart { kind: "text", text }],
        };
        self.post_json(
            &format!("/session/{session_id}/prompt_async"),
            directory,
            &body,
        )
        .await
    }

    async fn abort(&self, session_id: &str, directory: &str) -> Result<()> {
        self.post_empty(&format!("/session/{session_id}/abort"), directory)
            .await
    }

    async fn reply_permission(
        &self,
        _session_id: &str,
        directory: &str,
        request_id: &str,
        reply: PermissionReply,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            reply: &'a str,
        }
        self.post_json(
            &format!("/permission/{request_id}/reply"),
            directory,
            &Body {
                reply: reply.as_str(),
            },
        )
        .await
    }

    async fn reply_question(
        &self,
        _session_id: &str,
        directory: &str,
        request_id: &str,
        _request: &crate::opencode::types::QuestionRequest,
        answers: Vec<Vec<String>>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Body {
            answers: Vec<Vec<String>>,
        }
        self.post_json(
            &format!("/question/{request_id}/reply"),
            directory,
            &Body { answers },
        )
        .await
    }

    async fn event_stream(&self) -> Result<EventStream> {
        let request = self.event_request();
        let mut source = EventSource::new(request).context("failed to create SSE request")?;

        let stream = async_stream::try_stream! {
            while let Some(event) = source.next().await {
                match event {
                    Ok(Event::Open) => {
                        tracing::debug!("opencode event stream opened");
                    }
                    Ok(Event::Message(message)) => {
                        if message.data.trim().is_empty() {
                            continue;
                        }
                        let value: serde_json::Value = serde_json::from_str(&message.data)
                            .with_context(|| format!("invalid opencode event: {}", message.data))?;
                        let value = value.get("payload").cloned().unwrap_or(value);
                        let raw: RawEvent = serde_json::from_value(value)
                            .with_context(|| format!("invalid opencode event: {}", message.data))?;
                        yield raw;
                    }
                    Err(error) => {
                        Err(anyhow::anyhow!("opencode event stream error: {error}"))?;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn normalize_event(&self, raw: RawEvent) -> crate::opencode::backend::AgentEvent {
        crate::opencode::backend::normalize_v1_event(raw)
    }
}

fn directory_query(directory: &str) -> Vec<(&str, &str)> {
    if directory.is_empty() {
        Vec::new()
    } else {
        vec![("directory", directory)]
    }
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("opencode request failed with {status}: {body}");
    }
    response.json::<T>().await.map_err(Into::into)
}

async fn ensure_success(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    if status == StatusCode::NO_CONTENT {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    bail!("opencode request failed with {status}: {body}");
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        extract::Path,
        http::{StatusCode as AxumStatus, header},
        routing::{get, post},
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::{config::OpenCodeConfig, opencode::types::QuestionRequest};

    fn app() -> (Router, Arc<Mutex<Option<Value>>>) {
        let captured = Arc::new(Mutex::new(None));

        let prompt_captured = captured.clone();
        let prompt = post(move |Json(body): Json<Value>| {
            let captured = prompt_captured.clone();
            async move {
                *captured.lock().unwrap() = Some(body);
                AxumStatus::NO_CONTENT
            }
        });

        let router = Router::new()
            .route(
                "/global/event",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "data: {\"payload\":{\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"ses_1\"}}}\n\n",
                    )
                }),
            )
            .route(
                "/session",
                get(|| async {
                    Json(json!([{
                        "id": "ses_1",
                        "title": "Test session",
                        "directory": "/tmp",
                        "projectID": "prj_1"
                    }]))
                })
                .post(|| async {
                    Json(json!({
                        "id": "ses_2",
                        "title": "New session",
                        "directory": "/tmp/project",
                        "projectID": "prj_1"
                    }))
                }),
            )
            .route(
                "/session/status",
                get(|| async { Json(json!({"ses_1": {"type": "busy"}})) }),
            )
            .route(
                "/session/{session_id}/message",
                get(|| async {
                    Json(json!([{
                        "info": {
                            "id": "msg_1",
                            "role": "assistant",
                            "time": {"created": 1, "completed": 2}
                        },
                        "parts": [{"type": "text", "text": "done"}]
                    }]))
                }),
            )
            .route("/session/{session_id}/prompt_async", prompt)
            .route(
                "/session/{session_id}/abort",
                post(|| async { AxumStatus::NO_CONTENT }),
            )
            .route(
                "/permission/{request_id}/reply",
                post(|Path(_): Path<String>| async { AxumStatus::OK }),
            )
            .route(
                "/question/{request_id}/reply",
                post(|Path(_): Path<String>| async { AxumStatus::OK }),
            );

        (router, captured)
    }

    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}")
    }

    #[test]
    fn trims_base_url() {
        let mut config = OpenCodeConfig::default();
        config.url = "http://127.0.0.1:4096/".to_string();
        let client = V1Http::new(&config).unwrap();
        assert_eq!(client.base_url, "http://127.0.0.1:4096");
    }

    #[tokio::test]
    async fn forwards_directory_to_permission_reply() {
        use std::collections::HashMap;

        use axum::{extract::Query, routing::post};

        let captured = Arc::new(Mutex::new(None));
        let handler_state = captured.clone();
        let router = Router::new().route(
            "/permission/{request_id}/reply",
            post(move |Query(query): Query<HashMap<String, String>>| {
                let captured = handler_state.clone();
                async move {
                    *captured.lock().unwrap() = Some(query);
                    AxumStatus::OK
                }
            }),
        );
        let url = spawn(router).await;
        let config = OpenCodeConfig {
            url,
            ..Default::default()
        };
        let client = V1Http::new(&config).unwrap();

        client
            .reply_permission("ses_1", "/tmp/project", "per_1", PermissionReply::Once)
            .await
            .unwrap();

        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .get("directory")
                .map(String::as_str),
            Some("/tmp/project")
        );
    }

    #[tokio::test]
    async fn fetches_session_by_id() {
        let router = Router::new().route(
            "/session/{session_id}",
            get(|| async {
                Json(json!({
                    "id": "ses_1",
                    "title": "Test session",
                    "directory": "/tmp/project"
                }))
            }),
        );
        let url = spawn(router).await;
        let config = OpenCodeConfig {
            url,
            ..Default::default()
        };
        let client = V1Http::new(&config).unwrap();

        let session = client.get_session("ses_1").await.unwrap().unwrap();
        assert_eq!(session.id, "ses_1");
        assert_eq!(session.directory, "/tmp/project");
    }

    #[tokio::test]
    async fn talks_to_v1_api() {
        let (router, captured) = app();
        let url = spawn(router).await;
        let config = OpenCodeConfig {
            url,
            ..Default::default()
        };
        let client = V1Http::new(&config).unwrap();

        let sessions = client.list_sessions().await.unwrap();
        assert_eq!(sessions[0].id, "ses_1");
        assert_eq!(sessions[0].title, "Test session");

        let created = client.create_session("/tmp/project").await.unwrap();
        assert_eq!(created.id, "ses_2");
        assert_eq!(created.directory, "/tmp/project");

        let statuses = client.session_statuses().await.unwrap();
        assert_eq!(statuses.get("ses_1"), Some(&SessionStatus::Busy));

        let messages = client.messages("ses_1", "", 10).await.unwrap();
        assert_eq!(messages[0].text(), "done");

        client.prompt_async("ses_1", "", "hello").await.unwrap();
        assert_eq!(
            captured.lock().unwrap().as_ref().unwrap()["parts"][0]["text"],
            "hello"
        );

        client
            .reply_permission("ses_1", "", "per_1", PermissionReply::Once)
            .await
            .unwrap();
        client
            .reply_question(
                "ses_1",
                "",
                "que_1",
                &QuestionRequest {
                    id: "que_1".to_string(),
                    session_id: "ses_1".to_string(),
                    questions: Vec::new(),
                },
                vec![vec!["yes".to_string()]],
            )
            .await
            .unwrap();

        let mut stream = client.event_stream().await.unwrap();
        let raw = stream.next().await.unwrap().unwrap();
        assert_eq!(raw.event_type, "session.idle");
    }
}
