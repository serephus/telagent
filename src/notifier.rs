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
        body: Option<&str>,
        context: Option<&SessionContext>,
    ) -> Result<(String, Vec<(i64, i32)>)> {
        let text = render::completion_message(session, body, context);
        let markup = completion_markup(&session.session_id);
        let sent = self
            .deliver(&session.session_id, "completion", &text, Some(markup))
            .await?;
        Ok((text, sent))
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

        let mut text = text.to_string();
        if self.paste.should_paste(&text) {
            match self.paste.upload(&text).await {
                Ok(url) => {
                    text = format!("Full message is too long for Telegram:\n{url}");
                }
                Err(error) => {
                    tracing::warn!(%error, "paste upload failed; falling back to chunks");
                }
            }
        }

        let chunks = render::chunk_text(&text, self.config.notify.max_telegram_chars);
        let last_index = chunks.len().saturating_sub(1);

        for chat_id in chats {
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
                    && let Some(markup) = &markup
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
                    Err(error) => {
                        tracing::warn!(chat_id, %error, "failed to send telegram message");
                    }
                }
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
