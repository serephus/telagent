use anyhow::Result;
use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    prelude::Requester,
    types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, LinkPreviewOptions, MessageId},
};

use crate::{
    config::Config,
    opencode::types::{PermissionRequest, QuestionRequest, SessionContext},
    paste::PasteClient,
    render,
    state::{Db, SessionRecord},
};

const MAX_NOTIFICATION_ATTEMPTS: i64 = 50;

#[derive(Clone)]
pub struct Notifier {
    bot: Bot,
    config: Config,
    db: Db,
    paste: PasteClient,
    instance_id: String,
}

impl Notifier {
    pub fn new(bot: Bot, config: Config, db: Db, paste: PasteClient, instance_id: String) -> Self {
        Self {
            bot,
            config,
            db,
            paste,
            instance_id,
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
                &session.session_id,
            )
            .await;
        let markup = completion_markup(&session.session_id);
        let chats = self.db.bound_chats(&self.instance_id).await?;
        let mut sent = Vec::new();

        for chat_id in chats {
            let (key, created) = self
                .db
                .enqueue_notification(
                    chat_id,
                    &session.session_id,
                    message_id,
                    "completion",
                    &text,
                )
                .await?;
            if !created {
                continue;
            }
            match self
                .send_chat_text(
                    chat_id,
                    &session.session_id,
                    "completion",
                    &text,
                    Some(&markup),
                )
                .await
            {
                Ok(mut messages) => {
                    sent.append(&mut messages);
                    if let Err(error) = self.db.remove_notification(&key).await {
                        tracing::warn!(%error, chat_id, "failed to clear notification");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        chat_id,
                        session_id = %session.session_id,
                        %error,
                        "completion notification queued for retry"
                    );
                    if let Err(error) = self.db.increment_notification_attempts(&key).await {
                        tracing::warn!(%error, chat_id, "failed to reschedule notification");
                    }
                }
            }
        }

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

            let markup = match item.kind.as_str() {
                "completion" => Some(completion_markup(&item.session_id)),
                _ => None,
            };
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
                Ok(_) => {
                    let _ = self.db.remove_notification(&item.key).await;
                    if let Err(error) = self
                        .db
                        .set_last_notified_message(&item.session_id, &item.message_id)
                        .await
                    {
                        tracing::warn!(%error, "failed to record retried notification");
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        chat_id = item.chat_id,
                        session_id = %item.session_id,
                        attempts = item.attempts + 1,
                        %error,
                        "notification retry failed"
                    );
                    let attempts = self.db.increment_notification_attempts(&item.key).await?;
                    if attempts >= MAX_NOTIFICATION_ATTEMPTS {
                        tracing::warn!(
                            chat_id = item.chat_id,
                            session_id = %item.session_id,
                            "dropping notification after repeated failures"
                        );
                        self.db.remove_notification(&item.key).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn notify_error(
        &self,
        session: &SessionRecord,
        error: &str,
    ) -> Result<Vec<(i64, i32)>> {
        let text = render::error_message(session, error);
        let markup = completion_markup(&session.session_id);
        self.deliver(&session.session_id, "error", &text, Some(markup))
            .await
    }

    pub async fn notify_permission(
        &self,
        session: &SessionRecord,
        request: &PermissionRequest,
    ) -> Result<Vec<(i64, i32)>> {
        let text = render::permission_message(session, request);
        let markup = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("Allow once", format!("p:o:{}", request.id)),
            InlineKeyboardButton::callback("Always", format!("p:a:{}", request.id)),
            InlineKeyboardButton::callback("Deny", format!("p:r:{}", request.id)),
        ]]);
        let sent = self
            .deliver(&session.session_id, "permission", &text, Some(markup))
            .await?;

        let payload = serde_json::to_string(request)?;
        for (chat_id, message_id) in &sent {
            self.db
                .add_pending_request(
                    &request.id,
                    "permission",
                    &session.session_id,
                    Some(*chat_id),
                    Some(*message_id),
                    Some(&payload),
                )
                .await?;
        }
        Ok(sent)
    }

    pub async fn notify_question(
        &self,
        session: &SessionRecord,
        request: &QuestionRequest,
    ) -> Result<Vec<(i64, i32)>> {
        let text = render::question_message(session, request);
        let markup = question_markup(request);
        let sent = self
            .deliver(&session.session_id, "question", &text, markup)
            .await?;

        let payload = serde_json::to_string(request)?;
        for (chat_id, message_id) in &sent {
            self.db
                .add_pending_request(
                    &request.id,
                    "question",
                    &session.session_id,
                    Some(*chat_id),
                    Some(*message_id),
                    Some(&payload),
                )
                .await?;
        }
        Ok(sent)
    }

    pub async fn delete_telegram_message(&self, chat_id: Option<i64>, message_id: Option<i32>) {
        let (Some(chat_id), Some(message_id)) = (chat_id, message_id) else {
            return;
        };
        if let Err(error) = self
            .bot
            .delete_message(ChatId(chat_id), MessageId(message_id))
            .await
        {
            tracing::warn!(chat_id, message_id, %error, "failed to delete telegram message");
        }
    }

    pub async fn notify_created(&self, session: &SessionRecord) -> Result<Vec<(i64, i32)>> {
        let text = format!(
            "Session started\nTitle: {}\nDirectory: {}",
            render::session_title(session),
            session.directory
        );
        let markup = completion_markup(&session.session_id);
        self.deliver(&session.session_id, "created", &text, Some(markup))
            .await
    }

    async fn deliver(
        &self,
        session_id: &str,
        kind: &str,
        text: &str,
        markup: Option<InlineKeyboardMarkup>,
    ) -> Result<Vec<(i64, i32)>> {
        let mut sent = Vec::new();
        let chats = self.db.bound_chats(&self.instance_id).await?;
        if chats.is_empty() {
            tracing::debug!(session_id, "no bound telegram chats; notification dropped");
            return Ok(sent);
        }

        let text = self.prepare_text(text, kind, session_id).await;
        for chat_id in chats {
            match self
                .send_chat_text(chat_id, session_id, kind, &text, markup.as_ref())
                .await
            {
                Ok(mut messages) => sent.append(&mut messages),
                Err(error) => {
                    tracing::warn!(chat_id, %error, "failed to send telegram message");
                }
            }
        }

        Ok(sent)
    }

    async fn prepare_text(&self, text: &str, kind: &str, session_id: &str) -> String {
        let mut text = text.to_string();
        if self.paste.should_paste(&text) {
            let title = format!("telagent {kind} {session_id}");
            match self.paste.upload(&text, &title, None).await {
                Ok(url) => {
                    text = format!("Full message is too long for Telegram:\n{url}");
                }
                Err(error) => {
                    tracing::warn!(%error, "paste upload failed; falling back to chunks");
                }
            }
        }
        text
    }

    async fn send_chat_text(
        &self,
        chat_id: i64,
        session_id: &str,
        kind: &str,
        text: &str,
        markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<Vec<(i64, i32)>> {
        let chunks = render::chunk_text(text, self.config.notify.max_telegram_chars);
        let last_index = chunks.len().saturating_sub(1);
        let mut sent = Vec::new();

        for (index, chunk) in chunks.iter().enumerate() {
            let mut request = self
                .bot
                .send_message(ChatId(chat_id), chunk.clone())
                .link_preview_options(LinkPreviewOptions {
                    is_disabled: true,
                    url: None,
                    prefer_small_media: false,
                    prefer_large_media: false,
                    show_above_text: false,
                });
            if index == last_index
                && let Some(markup) = markup
            {
                request = request.reply_markup(markup.clone());
            }
            match request.await {
                Ok(message) => {
                    let message_id = message.id.0;
                    sent.push((chat_id, message_id));
                    let _ = self
                        .db
                        .store_telegram_message(chat_id, message_id, session_id, kind)
                        .await;
                }
                Err(error) => return Err(error.into()),
            }
        }

        Ok(sent)
    }
}

fn completion_markup(session_id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Reply", format!("r:{session_id}")),
        InlineKeyboardButton::callback("Mute", format!("m:{session_id}")),
        InlineKeyboardButton::callback("Sessions", "s".to_string()),
    ]])
}

fn question_markup(request: &QuestionRequest) -> Option<InlineKeyboardMarkup> {
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
        current.push(InlineKeyboardButton::callback(
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
    Some(InlineKeyboardMarkup::new(rows))
}
