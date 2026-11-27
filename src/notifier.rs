use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    prelude::Requester,
    types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, LinkPreviewOptions, MessageId},
};

use crate::{
    config::Config,
    metrics::RuntimeStatus,
    opencode::{
        OpenCodeBackend,
        types::{PermissionRequest, QuestionRequest, SessionContext},
    },
    paste::PasteClient,
    render,
    state::{Db, NewNotification, NotificationOutboxItem, SessionRecord},
};

const MAX_NOTIFICATION_ATTEMPTS: i64 = 50;

#[async_trait]
trait TelegramTransport: Send + Sync {
    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        markup: Option<&NotificationMarkup>,
    ) -> Result<i32>;

    async fn delete_message(&self, chat_id: i64, message_id: i32) -> Result<()>;
}

struct BotTransport {
    bot: Bot,
}

#[async_trait]
impl TelegramTransport for BotTransport {
    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        markup: Option<&NotificationMarkup>,
    ) -> Result<i32> {
        let mut request = self
            .bot
            .send_message(ChatId(chat_id), text.to_string())
            .link_preview_options(LinkPreviewOptions {
                is_disabled: true,
                url: None,
                prefer_small_media: false,
                prefer_large_media: false,
                show_above_text: false,
            });
        if let Some(markup) = markup {
            request = request.reply_markup(inline_markup(markup));
        }
        Ok(request.await?.id.0)
    }

    async fn delete_message(&self, chat_id: i64, message_id: i32) -> Result<()> {
        self.bot
            .delete_message(ChatId(chat_id), MessageId(message_id))
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Notifier {
    transport: Arc<dyn TelegramTransport>,
    config: Config,
    db: Db,
    backend: Arc<dyn OpenCodeBackend>,
    paste: PasteClient,
    instance_id: String,
    runtime: Arc<RuntimeStatus>,
}

impl Notifier {
    pub fn new(
        bot: Bot,
        config: Config,
        db: Db,
        backend: Arc<dyn OpenCodeBackend>,
        paste: PasteClient,
        instance_id: String,
        runtime: Arc<RuntimeStatus>,
    ) -> Self {
        Self {
            transport: Arc::new(BotTransport { bot }),
            config,
            db,
            backend,
            paste,
            instance_id,
            runtime,
        }
    }

    pub async fn notify_completion(
        &self,
        session: &SessionRecord,
        message_id: &str,
        body: Option<&str>,
        context: Option<&SessionContext>,
    ) -> Result<(String, Vec<(i64, i32)>)> {
        let text = self
            .prepare_text(
                &render::completion_message(session, body, context),
                "completion",
                session,
                context,
            )
            .await;
        let markup = completion_markup(&session.session_id);
        let sent = self
            .notify_chats(
                &session.session_id,
                message_id,
                "completion",
                &text,
                Some(markup),
                None,
            )
            .await?;
        Ok((text, sent))
    }

    pub async fn retry_notifications(&self, limit: usize) -> Result<()> {
        let items = self.db.list_notifications(limit).await?;
        let now = crate::state::now();
        for item in items {
            if item.next_attempt_at > now {
                continue;
            }
            if !self.db.is_bound(item.chat_id).await? {
                let _ = self.db.remove_notification(&item.key).await;
                continue;
            }

            self.runtime.inc_notification_retry();
            if matches!(item.kind.as_str(), "permission" | "question") {
                match self.request_still_pending(&item).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!(
                            session_id = %item.session_id,
                            request_id = %item.message_id,
                            kind = %item.kind,
                            "dropping notification for a resolved request"
                        );
                        self.db.remove_notification(&item.key).await?;
                        continue;
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        tracing::warn!(
                            session_id = %item.session_id,
                            request_id = %item.message_id,
                            %error,
                            "failed to validate pending request"
                        );
                        let attempts = self.db.increment_notification_attempts(&item.key).await?;
                        if attempts >= MAX_NOTIFICATION_ATTEMPTS {
                            tracing::error!(
                                session_id = %item.session_id,
                                request_id = %item.message_id,
                                %error,
                                "dropping unvalidated notification"
                            );
                            if let Err(error) =
                                self.db.record_dead_notification(&item, &reason).await
                            {
                                tracing::warn!(%error, "failed to record dead notification");
                            }
                            self.runtime.inc_notification_drop();
                            self.db.remove_notification(&item.key).await?;
                        }
                        continue;
                    }
                }
            }

            let markup = item
                .markup
                .as_deref()
                .and_then(|markup| serde_json::from_str::<NotificationMarkup>(markup).ok());
            match self
                .send_chat_text(
                    item.chat_id,
                    &item.session_id,
                    &item.kind,
                    &item.text,
                    markup.as_ref(),
                )
                .await
            {
                Ok(messages) => {
                    if let Err(error) = self
                        .record_delivery(
                            &item.session_id,
                            &item.message_id,
                            &item.kind,
                            item.payload.as_deref(),
                            &messages,
                        )
                        .await
                    {
                        tracing::warn!(%error, "failed to record retried notification");
                    }
                    let _ = self.db.remove_notification(&item.key).await;
                }
                Err(error) => {
                    let reason = error.to_string();
                    tracing::warn!(
                        chat_id = item.chat_id,
                        session_id = %item.session_id,
                        attempts = item.attempts + 1,
                        %error,
                        "notification retry failed"
                    );
                    let attempts = self.db.increment_notification_attempts(&item.key).await?;
                    if attempts >= MAX_NOTIFICATION_ATTEMPTS {
                        tracing::error!(
                            chat_id = item.chat_id,
                            session_id = %item.session_id,
                            "dropping notification after repeated failures"
                        );
                        if let Err(error) = self.db.record_dead_notification(&item, &reason).await {
                            tracing::warn!(%error, "failed to record dead notification");
                        }
                        self.runtime.inc_notification_drop();
                        self.db.remove_notification(&item.key).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn request_still_pending(&self, item: &NotificationOutboxItem) -> Result<bool> {
        let Some(session) = self
            .db
            .get_session(&self.instance_id, &item.session_id)
            .await?
        else {
            return Ok(false);
        };
        let mut directory = session.directory;
        if directory.is_empty()
            && let Some(info) = self.backend.get_session(&item.session_id).await?
        {
            directory = info.directory;
        }

        let pending = match item.kind.as_str() {
            "permission" => {
                self.backend
                    .pending_permission_ids(&item.session_id, &directory)
                    .await?
            }
            "question" => {
                self.backend
                    .pending_question_ids(&item.session_id, &directory)
                    .await?
            }
            _ => return Ok(true),
        };
        Ok(request_is_pending(pending, &item.message_id))
    }

    pub async fn notify_error(
        &self,
        session: &SessionRecord,
        error: &str,
    ) -> Result<Vec<(i64, i32)>> {
        let text = self
            .prepare_text(
                &render::error_message(session, error),
                "error",
                session,
                None,
            )
            .await;
        let dedupe_id = unique_notification_id("error");
        self.notify_chats(
            &session.session_id,
            &dedupe_id,
            "error",
            &text,
            Some(completion_markup(&session.session_id)),
            None,
        )
        .await
    }

    pub async fn notify_permission(
        &self,
        session: &SessionRecord,
        request: &PermissionRequest,
    ) -> Result<Vec<(i64, i32)>> {
        let text = self
            .prepare_text(
                &render::permission_message(session, request),
                "permission",
                session,
                None,
            )
            .await;
        let payload = serde_json::to_string(request)?;
        self.notify_chats(
            &session.session_id,
            &request.id,
            "permission",
            &text,
            Some(permission_markup(&request.id)),
            Some(payload),
        )
        .await
    }

    pub async fn notify_question(
        &self,
        session: &SessionRecord,
        request: &QuestionRequest,
    ) -> Result<Vec<(i64, i32)>> {
        let text = self
            .prepare_text(
                &render::question_message(session, request),
                "question",
                session,
                None,
            )
            .await;
        let payload = serde_json::to_string(request)?;
        self.notify_chats(
            &session.session_id,
            &request.id,
            "question",
            &text,
            question_markup(request),
            Some(payload),
        )
        .await
    }

    pub async fn delete_telegram_message(&self, chat_id: Option<i64>, message_id: Option<i32>) {
        let (Some(chat_id), Some(message_id)) = (chat_id, message_id) else {
            return;
        };
        if let Err(error) = self.transport.delete_message(chat_id, message_id).await {
            tracing::warn!(chat_id, message_id, %error, "failed to delete telegram message");
        }
    }

    pub async fn notify_created(&self, session: &SessionRecord) -> Result<Vec<(i64, i32)>> {
        let text = self
            .prepare_text(
                &format!(
                    "Session started\nTitle: {}\nDirectory: {}",
                    render::session_title(session),
                    session.directory
                ),
                "created",
                session,
                None,
            )
            .await;
        let dedupe_id = unique_notification_id("created");
        self.notify_chats(
            &session.session_id,
            &dedupe_id,
            "created",
            &text,
            Some(completion_markup(&session.session_id)),
            None,
        )
        .await
    }

    pub async fn notify_system(&self, text: &str) -> Result<()> {
        self.db.remove_notifications_by_kind("system").await?;
        let dedupe_id = unique_notification_id("system");
        self.notify_chats("", &dedupe_id, "system", text, None, None)
            .await?;
        Ok(())
    }

    async fn notify_chats(
        &self,
        session_id: &str,
        dedupe_id: &str,
        kind: &str,
        text: &str,
        markup: Option<NotificationMarkup>,
        payload: Option<String>,
    ) -> Result<Vec<(i64, i32)>> {
        let mut sent = Vec::new();
        let chats = self.db.bound_chats(&self.instance_id).await?;
        if chats.is_empty() {
            tracing::debug!(session_id, "no bound telegram chats; notification dropped");
            return Ok(sent);
        }

        let markup_json = markup.as_ref().map(serde_json::to_string).transpose()?;
        for chat_id in chats {
            let (key, created) = self
                .db
                .enqueue_notification(NewNotification {
                    chat_id,
                    session_id,
                    message_id: dedupe_id,
                    kind,
                    text,
                    markup: markup_json.as_deref(),
                    payload: payload.as_deref(),
                })
                .await?;
            if !created {
                continue;
            }
            match self
                .send_chat_text(chat_id, session_id, kind, text, markup.as_ref())
                .await
            {
                Ok(mut messages) => {
                    if let Err(error) = self
                        .record_delivery(session_id, dedupe_id, kind, payload.as_deref(), &messages)
                        .await
                    {
                        tracing::warn!(%error, chat_id, "failed to record delivery");
                    }
                    sent.append(&mut messages);
                    if let Err(error) = self.db.remove_notification(&key).await {
                        tracing::warn!(%error, chat_id, "failed to clear notification");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        chat_id,
                        session_id,
                        %error,
                        "notification queued for retry"
                    );
                    if let Err(error) = self.db.increment_notification_attempts(&key).await {
                        tracing::warn!(%error, chat_id, "failed to reschedule notification");
                    }
                }
            }
        }

        Ok(sent)
    }

    async fn record_delivery(
        &self,
        session_id: &str,
        dedupe_id: &str,
        kind: &str,
        payload: Option<&str>,
        messages: &[(i64, i32)],
    ) -> Result<()> {
        match kind {
            "completion" => {
                self.db
                    .set_last_notified_message(session_id, dedupe_id)
                    .await?;
            }
            "permission" | "question" => {
                for (chat_id, message_id) in messages {
                    self.db
                        .add_pending_request(
                            dedupe_id,
                            kind,
                            session_id,
                            Some(*chat_id),
                            Some(*message_id),
                            payload,
                        )
                        .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn prepare_text(
        &self,
        text: &str,
        kind: &str,
        session: &SessionRecord,
        context: Option<&SessionContext>,
    ) -> String {
        if self.paste.should_paste(text) {
            let title = format!("telagent {kind} {}", session.session_id);
            match self.paste.upload(text, &title, None).await {
                Ok(url) => {
                    return format!(
                        "{}\n\nFull message is too long for Telegram:\n{url}",
                        render::session_metadata(kind, session, context)
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "paste upload failed; falling back to chunks");
                }
            }
        }
        text.to_string()
    }

    async fn send_chat_text(
        &self,
        chat_id: i64,
        session_id: &str,
        kind: &str,
        text: &str,
        markup: Option<&NotificationMarkup>,
    ) -> Result<Vec<(i64, i32)>> {
        let chunks = render::chunk_text(text, self.config.notify.max_telegram_chars);
        let last_index = chunks.len().saturating_sub(1);
        let mut sent = Vec::new();

        for (index, chunk) in chunks.iter().enumerate() {
            let markup = (index == last_index).then_some(markup).flatten();
            match self.transport.send_message(chat_id, chunk, markup).await {
                Ok(message_id) => {
                    sent.push((chat_id, message_id));
                    if !session_id.is_empty() {
                        let _ = self
                            .db
                            .store_telegram_message(chat_id, message_id, session_id, kind)
                            .await;
                    }
                }
                Err(error) => return Err(error),
            }
        }

        Ok(sent)
    }
}

fn unique_notification_id(kind: &str) -> String {
    format!("{kind}:{}", uuid::Uuid::new_v4())
}

fn request_is_pending(
    pending: Option<std::collections::HashSet<String>>,
    request_id: &str,
) -> bool {
    pending
        .map(|pending| pending.contains(request_id))
        .unwrap_or(true)
}

type NotificationMarkup = Vec<Vec<(String, String)>>;

fn completion_markup(session_id: &str) -> NotificationMarkup {
    vec![vec![
        ("Reply".to_string(), format!("r:{session_id}")),
        ("Mute".to_string(), format!("m:{session_id}")),
        ("Sessions".to_string(), "s".to_string()),
    ]]
}

fn permission_markup(request_id: &str) -> NotificationMarkup {
    vec![vec![
        ("Allow once".to_string(), format!("p:o:{request_id}")),
        ("Always".to_string(), format!("p:a:{request_id}")),
        ("Deny".to_string(), format!("p:r:{request_id}")),
    ]]
}

fn question_markup(request: &QuestionRequest) -> Option<NotificationMarkup> {
    if request.questions.len() != 1 {
        return None;
    }
    let question = &request.questions[0];
    if question.options.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    let mut current = Vec::new();
    for (index, option) in question.options.iter().enumerate() {
        current.push((
            render::truncate(&option.label, 40),
            format!("q:0:{index}:{}", request.id),
        ));
        if current.len() == 2 {
            rows.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    Some(rows)
}

fn inline_markup(markup: &NotificationMarkup) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(
        markup
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(text, callback_data)| {
                        InlineKeyboardButton::callback(text.clone(), callback_data.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    use anyhow::anyhow;
    use axum::{Json, Router, routing::get};
    use serde_json::json;

    use super::*;
    use crate::{
        config::{Config, OpenCodeConfig, PasteConfig},
        metrics::RuntimeStatus,
        opencode::v1_http::V1Http,
        state::{NewNotification, SessionRecord},
    };

    #[derive(Clone, Debug)]
    struct SentMessage {
        text: String,
        markup: Option<NotificationMarkup>,
    }

    struct FakeState {
        fail_next: bool,
        sends: Vec<SentMessage>,
        deleted: Vec<(i64, i32)>,
    }

    struct FakeTransport {
        state: Mutex<FakeState>,
    }

    impl FakeTransport {
        fn new(fail_next: bool) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    fail_next,
                    sends: Vec::new(),
                    deleted: Vec::new(),
                }),
            }
        }

        fn sends(&self) -> Vec<SentMessage> {
            self.state.lock().unwrap().sends.clone()
        }
    }

    #[async_trait::async_trait]
    impl TelegramTransport for FakeTransport {
        async fn send_message(
            &self,
            _chat_id: i64,
            text: &str,
            markup: Option<&NotificationMarkup>,
        ) -> Result<i32> {
            let mut state = self.state.lock().unwrap();
            if state.fail_next {
                state.fail_next = false;
                return Err(anyhow!("network down"));
            }
            state.sends.push(SentMessage {
                text: text.to_string(),
                markup: markup.cloned(),
            });
            Ok(1000 + state.sends.len() as i32)
        }

        async fn delete_message(&self, chat_id: i64, message_id: i32) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .deleted
                .push((chat_id, message_id));
            Ok(())
        }
    }

    async fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.redb")).unwrap();
        db.bind("i", 1, 1).await.unwrap();
        (dir, db)
    }

    fn v1_backend(url: String) -> Arc<dyn OpenCodeBackend> {
        Arc::new(
            V1Http::new(&OpenCodeConfig {
                url,
                ..Default::default()
            })
            .unwrap(),
        )
    }

    fn notifier(
        db: Db,
        transport: Arc<FakeTransport>,
        backend: Arc<dyn OpenCodeBackend>,
    ) -> Notifier {
        Notifier {
            transport,
            config: Config::default(),
            db,
            backend,
            paste: PasteClient::new(&PasteConfig::default()).unwrap(),
            instance_id: "i".to_string(),
            runtime: Arc::new(RuntimeStatus::default()),
        }
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
    fn pending_request_check_handles_unknown_and_resolved_requests() {
        assert!(request_is_pending(None, "req_1"));
        assert!(request_is_pending(
            Some(HashSet::from(["req_1".to_string()])),
            "req_1"
        ));
        assert!(!request_is_pending(
            Some(HashSet::from(["req_2".to_string()])),
            "req_1"
        ));
    }

    #[tokio::test]
    async fn retries_completion_and_restores_markup() {
        let (_dir, db) = test_db().await;
        let markup = serde_json::to_string(&completion_markup("ses_1")).unwrap();
        let (key, _) = db
            .enqueue_notification(NewNotification {
                chat_id: 1,
                session_id: "ses_1",
                message_id: "msg_1",
                kind: "completion",
                text: "done",
                markup: Some(&markup),
                payload: None,
            })
            .await
            .unwrap();
        db.force_notification_due(&key).await.unwrap();

        let transport = Arc::new(FakeTransport::new(true));
        let notifier = notifier(
            db.clone(),
            transport.clone(),
            v1_backend("http://127.0.0.1:1".to_string()),
        );
        notifier.retry_notifications(10).await.unwrap();
        assert_eq!(db.list_notifications(10).await.unwrap().len(), 1);

        db.force_notification_due(&key).await.unwrap();
        notifier.retry_notifications(10).await.unwrap();
        assert!(db.list_notifications(10).await.unwrap().is_empty());
        let sends = transport.sends();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].text, "done");
        assert_eq!(sends[0].markup, Some(completion_markup("ses_1")));
    }

    #[tokio::test]
    async fn recreates_pending_request_and_drops_resolved_one() {
        let router = Router::new().route(
            "/permission",
            get(|| async {
                Json(json!([{
                    "id": "per_1",
                    "sessionID": "ses_1",
                    "permission": "read"
                }]))
            }),
        );
        let backend = v1_backend(spawn(router).await);
        let (_dir, db) = test_db().await;
        db.upsert_session(&SessionRecord {
            instance_id: "i".to_string(),
            session_id: "ses_1".to_string(),
            title: "Test".to_string(),
            directory: "/tmp".to_string(),
            project_id: None,
            parent_id: None,
            state: "idle".to_string(),
            last_assistant_message_id: None,
            last_notified_message_id: None,
            notification_pending: false,
            muted: false,
            updated_at: 0,
        })
        .await
        .unwrap();
        let payload = serde_json::to_string(&PermissionRequest {
            id: "per_1".to_string(),
            session_id: "ses_1".to_string(),
            permission: "read".to_string(),
            patterns: Vec::new(),
            metadata: serde_json::Value::Null,
            always: Vec::new(),
        })
        .unwrap();
        let markup = serde_json::to_string(&permission_markup("per_1")).unwrap();
        let (key, _) = db
            .enqueue_notification(NewNotification {
                chat_id: 1,
                session_id: "ses_1",
                message_id: "per_1",
                kind: "permission",
                text: "permission",
                markup: Some(&markup),
                payload: Some(&payload),
            })
            .await
            .unwrap();
        db.force_notification_due(&key).await.unwrap();

        let transport = Arc::new(FakeTransport::new(false));
        let notifier = notifier(db.clone(), transport.clone(), backend);
        notifier.retry_notifications(10).await.unwrap();
        assert!(db.list_notifications(10).await.unwrap().is_empty());
        assert!(
            db.get_pending_request_for_chat("per_1", 1)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(transport.sends().len(), 1);

        let (key, _) = db
            .enqueue_notification(NewNotification {
                chat_id: 1,
                session_id: "ses_1",
                message_id: "per_2",
                kind: "permission",
                text: "stale",
                markup: None,
                payload: Some(&payload),
            })
            .await
            .unwrap();
        db.force_notification_due(&key).await.unwrap();
        notifier.retry_notifications(10).await.unwrap();
        assert!(db.list_notifications(10).await.unwrap().is_empty());
        assert_eq!(transport.sends().len(), 1);
    }

    #[tokio::test]
    async fn system_notice_uses_transport() {
        let (_dir, db) = test_db().await;
        let transport = Arc::new(FakeTransport::new(false));
        let notifier = notifier(
            db.clone(),
            transport.clone(),
            v1_backend("http://127.0.0.1:1".to_string()),
        );
        notifier
            .notify_system("opencode is unreachable.")
            .await
            .unwrap();
        assert!(db.list_notifications(10).await.unwrap().is_empty());
        let sends = transport.sends();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].text, "opencode is unreachable.");
    }
}
