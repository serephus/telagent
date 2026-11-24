use std::sync::Arc;

use anyhow::Result;
use teloxide::{
    Bot,
    prelude::*,
    types::{
        CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup,
        MaybeInaccessibleMessage, Message, Update, UpdateKind,
    },
};

use crate::{
    app::AppState,
    opencode::types::{PermissionReply, QuestionRequest},
    pairing, render,
};

pub type HandlerResult = ResponseResult<()>;

pub async fn handle_update(bot: Bot, update: Update, state: Arc<AppState>) -> HandlerResult {
    let update_id = update.id.0 as i64;
    if state
        .db
        .is_processed_update(update_id)
        .await
        .map_err(response_error)?
    {
        return Ok(());
    }
    state
        .db
        .mark_update(update_id)
        .await
        .map_err(response_error)?;

    match &update.kind {
        UpdateKind::Message(message) => {
            handle_message(&bot, &state, message)
                .await
                .map_err(response_error)?;
        }
        UpdateKind::CallbackQuery(callback) => {
            handle_callback(&bot, &state, callback)
                .await
                .map_err(response_error)?;
        }
        _ => {}
    }
    Ok(())
}

fn response_error(error: anyhow::Error) -> teloxide::RequestError {
    teloxide::RequestError::Io(std::sync::Arc::new(std::io::Error::other(
        error.to_string(),
    )))
}

async fn handle_message(bot: &Bot, state: &AppState, message: &Message) -> Result<()> {
    let chat_id = message.chat.id;
    let user_id = message.from.as_ref().map(|user| user.id.0).unwrap_or(0);

    if !state.is_allowed_user(user_id) {
        send(bot, chat_id, "Not authorized.").await?;
        return Ok(());
    }

    let bound = state.db.is_bound(chat_id.0).await?;

    if let Some(text) = message.text()
        && let Some((command, args)) = parse_command(text)
    {
        handle_command(bot, state, message, command, &args, bound).await?;
        return Ok(());
    }

    if state.config.security.require_bind && !bound {
        send(
            bot,
            chat_id,
            "This chat is not bound. Run `telagent pair` and send /bind CODE.",
        )
        .await?;
        return Ok(());
    }

    let text = message.text().unwrap_or("").trim();
    if text.is_empty() {
        return Ok(());
    }

    if let Some(reply) = message.reply_to_message() {
        let replied_message_id = reply.id.0;

        if let Some(pending) = state
            .db
            .pending_request_for_message(chat_id.0, replied_message_id)
            .await?
            && pending.kind == "question"
        {
            answer_pending_question(bot, state, chat_id, &pending, text).await?;
            return Ok(());
        }

        if let Some(session_id) = state
            .db
            .session_for_telegram_message(chat_id.0, replied_message_id)
            .await?
        {
            send_feedback(bot, state, chat_id, &session_id, text).await?;
            return Ok(());
        }
    }

    if let Some(session_id) = state.db.active_session(chat_id.0).await? {
        send_feedback(bot, state, chat_id, &session_id, text).await?;
    } else {
        send(
            bot,
            chat_id,
            "No active session. Reply to a notification or use /sessions then /use ID.",
        )
        .await?;
    }

    Ok(())
}

async fn handle_command(
    bot: &Bot,
    state: &AppState,
    message: &Message,
    command: &str,
    args: &[&str],
    bound: bool,
) -> Result<()> {
    let chat_id = message.chat.id;
    let user_id = message.from.as_ref().map(|user| user.id.0).unwrap_or(0);

    match command {
        "start" | "help" => {
            send(
                bot,
                chat_id,
                "telagent\n\nCommands:\n/bind CODE\n/unbind\n/sessions\n/use ID\n/status\n/mute [ID]\n/unmute [ID]\n/abort [ID]\n/help\n\nReply to a notification to send feedback.",
            )
            .await?;
        }
        "bind" => {
            let Some(code) = args.first() else {
                send(bot, chat_id, "Usage: /bind CODE").await?;
                return Ok(());
            };
            match pairing::consume_pairing_code(&state.db, code).await? {
                Some(instance_id) if instance_id == state.instance_id => {
                    state
                        .db
                        .bind(&state.instance_id, chat_id.0, user_id as i64)
                        .await?;
                    send(
                        bot,
                        chat_id,
                        "Bound this chat to the local opencode instance.",
                    )
                    .await?;
                }
                Some(_) => {
                    send(bot, chat_id, "That code belongs to a different instance.").await?;
                }
                None => {
                    send(bot, chat_id, "Invalid or expired code.").await?;
                }
            }
        }
        _ if !bound && state.config.security.require_bind => {
            send(
                bot,
                chat_id,
                "This chat is not bound. Run `telagent pair` and send /bind CODE.",
            )
            .await?;
        }
        "unbind" => {
            state.db.unbind(chat_id.0).await?;
            send(bot, chat_id, "Unbound this chat.").await?;
        }
        "sessions" => {
            send_sessions(bot, state, chat_id).await?;
        }
        "status" => {
            let sessions = state.db.list_sessions(&state.instance_id, 10).await?;
            send(bot, chat_id, render::sessions_message(&sessions)).await?;
        }
        "use" => {
            let Some(session_id) = args.first() else {
                send(bot, chat_id, "Usage: /use SESSION_ID").await?;
                return Ok(());
            };
            if state
                .db
                .get_session(&state.instance_id, session_id)
                .await?
                .is_some()
            {
                state.db.set_active_session(chat_id.0, session_id).await?;
                send(bot, chat_id, format!("Active session: {session_id}")).await?;
            } else {
                send(bot, chat_id, "Unknown session.").await?;
            }
        }
        "mute" | "unmute" => {
            let Some(session_id) = resolve_session_id(state, chat_id.0, args).await? else {
                send(bot, chat_id, "No active session. Pass a session ID.").await?;
                return Ok(());
            };
            let muted = command == "mute";
            state.db.set_muted(&session_id, muted).await?;
            send(
                bot,
                chat_id,
                format!(
                    "{} notifications for {session_id}.",
                    if muted { "Muted" } else { "Unmuted" }
                ),
            )
            .await?;
        }
        "abort" => {
            let Some(session_id) = resolve_session_id(state, chat_id.0, args).await? else {
                send(bot, chat_id, "No active session. Pass a session ID.").await?;
                return Ok(());
            };
            match state.backend.abort(&session_id).await {
                Ok(()) => {
                    send(bot, chat_id, format!("Aborted {session_id}.")).await?;
                }
                Err(error) => {
                    send(bot, chat_id, format!("Abort failed: {error}")).await?;
                }
            }
        }
        other => {
            send(bot, chat_id, format!("Unknown command: /{other}")).await?;
        }
    }

    Ok(())
}

async fn handle_callback(bot: &Bot, state: &AppState, callback: &CallbackQuery) -> Result<()> {
    let user_id = callback.from.id.0;
    let chat_id = callback_chat_id(callback);
    let _ = bot.answer_callback_query(callback.id.clone()).await;

    if !state.is_allowed_user(user_id) {
        send(bot, ChatId(chat_id), "Not authorized.").await?;
        return Ok(());
    }
    if state.config.security.require_bind && !state.db.is_bound(chat_id).await? {
        send(bot, ChatId(chat_id), "This chat is not bound.").await?;
        return Ok(());
    }

    let data = callback.data.as_deref().unwrap_or("");
    let parts: Vec<&str> = data.split(':').collect();
    match parts.as_slice() {
        ["r", session_id] => {
            state.db.set_active_session(chat_id, session_id).await?;
            send(
                bot,
                ChatId(chat_id),
                format!("Session selected: {session_id}\nSend your feedback now."),
            )
            .await?;
        }
        ["m", session_id] => {
            state.db.set_muted(session_id, true).await?;
            send(bot, ChatId(chat_id), format!("Muted {session_id}.")).await?;
        }
        ["u", session_id] => {
            state.db.set_active_session(chat_id, session_id).await?;
            send(
                bot,
                ChatId(chat_id),
                format!("Active session: {session_id}"),
            )
            .await?;
        }
        ["s"] => {
            send_sessions(bot, state, ChatId(chat_id)).await?;
        }
        ["p", action, request_id] => {
            let reply = match *action {
                "o" => PermissionReply::Once,
                "a" => PermissionReply::Always,
                "r" => PermissionReply::Reject,
                _ => {
                    send(bot, ChatId(chat_id), "Unknown permission action.").await?;
                    return Ok(());
                }
            };
            let Some(pending) = state
                .db
                .get_pending_request_for_chat(request_id, chat_id)
                .await?
            else {
                send(bot, ChatId(chat_id), "That request has expired.").await?;
                return Ok(());
            };
            if let Some(expected_chat) = pending.chat_id
                && expected_chat != chat_id
            {
                send(
                    bot,
                    ChatId(chat_id),
                    "This request belongs to another chat.",
                )
                .await?;
                return Ok(());
            }
            match state
                .backend
                .reply_permission(&pending.session_id, request_id, reply)
                .await
            {
                Ok(()) => {
                    let _ = state.db.remove_pending_request(request_id).await;
                    send(
                        bot,
                        ChatId(chat_id),
                        format!(
                            "Permission response sent: {}\nSession: {}",
                            reply.as_str(),
                            pending.session_id
                        ),
                    )
                    .await?;
                }
                Err(error) => {
                    send(bot, ChatId(chat_id), format!("Permission failed: {error}")).await?;
                }
            }
        }
        ["q", question_index, option_index, request_id] => {
            let Some(pending) = state
                .db
                .get_pending_request_for_chat(request_id, chat_id)
                .await?
            else {
                send(bot, ChatId(chat_id), "That question has expired.").await?;
                return Ok(());
            };
            if let Some(expected_chat) = pending.chat_id
                && expected_chat != chat_id
            {
                send(
                    bot,
                    ChatId(chat_id),
                    "This request belongs to another chat.",
                )
                .await?;
                return Ok(());
            }
            let Some(payload) = pending.payload else {
                send(bot, ChatId(chat_id), "Question payload is missing.").await?;
                return Ok(());
            };
            let request: QuestionRequest = serde_json::from_str(&payload)?;
            let question_index: usize = question_index.parse().unwrap_or(0);
            let option_index: usize = option_index.parse().unwrap_or(0);
            let Some(question) = request.questions.get(question_index) else {
                send(bot, ChatId(chat_id), "Question index is invalid.").await?;
                return Ok(());
            };
            let Some(option) = question.options.get(option_index) else {
                send(bot, ChatId(chat_id), "Option index is invalid.").await?;
                return Ok(());
            };

            let mut answers = vec![Vec::new(); request.questions.len()];
            answers[question_index] =
                vec![option.value.clone().unwrap_or_else(|| option.label.clone())];
            match state
                .backend
                .reply_question(&pending.session_id, request_id, &request, answers)
                .await
            {
                Ok(()) => {
                    let _ = state.db.remove_pending_request(request_id).await;
                    send(
                        bot,
                        ChatId(chat_id),
                        format!(
                            "Answer sent: {}\nSession: {}",
                            option.label, pending.session_id
                        ),
                    )
                    .await?;
                }
                Err(error) => {
                    send(
                        bot,
                        ChatId(chat_id),
                        format!("Question reply failed: {error}"),
                    )
                    .await?;
                }
            }
        }
        _ => {
            send(bot, ChatId(chat_id), "Unknown action.").await?;
        }
    }

    Ok(())
}

async fn send_feedback(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    session_id: &str,
    text: &str,
) -> Result<()> {
    match state.backend.prompt_async(session_id, text).await {
        Ok(()) => {
            send(bot, chat_id, format!("Sent to {session_id}.")).await?;
        }
        Err(error) => {
            state.db.enqueue_outbox(session_id, text).await?;
            send(
                bot,
                chat_id,
                format!(
                    "Could not deliver immediately ({error}). Queued and will retry when the session is available."
                ),
            )
            .await?;
        }
    }
    Ok(())
}

async fn answer_pending_question(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    pending: &crate::state::PendingRequest,
    text: &str,
) -> Result<()> {
    let Some(payload) = &pending.payload else {
        send(bot, chat_id, "Question payload is missing.").await?;
        return Ok(());
    };
    let request: QuestionRequest = serde_json::from_str(payload)?;
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect();

    let answers = if request.questions.len() == 1 {
        let question = &request.questions[0];
        if question.multiple {
            let values = text
                .split(['\n', ','])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            vec![values]
        } else {
            vec![vec![text.trim().to_string()]]
        }
    } else {
        if lines.len() < request.questions.len() {
            send(
                bot,
                chat_id,
                format!(
                    "This request has {} questions. Send one answer per line.",
                    request.questions.len()
                ),
            )
            .await?;
            return Ok(());
        }
        lines
            .into_iter()
            .take(request.questions.len())
            .map(|line| vec![line])
            .collect()
    };

    match state
        .backend
        .reply_question(&pending.session_id, &pending.request_id, &request, answers)
        .await
    {
        Ok(()) => {
            let _ = state.db.remove_pending_request(&pending.request_id).await;
            send(bot, chat_id, "Answer sent.").await?;
        }
        Err(error) => {
            send(bot, chat_id, format!("Question reply failed: {error}")).await?;
        }
    }
    Ok(())
}

async fn send_sessions(bot: &Bot, state: &AppState, chat_id: ChatId) -> Result<()> {
    let sessions = state.db.list_sessions(&state.instance_id, 10).await?;
    let text = render::sessions_message(&sessions);
    if sessions.is_empty() {
        send(bot, chat_id, text).await?;
        return Ok(());
    }

    let rows = sessions
        .iter()
        .map(|session| {
            vec![InlineKeyboardButton::callback(
                render::truncate(render::session_title(session), 40),
                format!("u:{}", session.session_id),
            )]
        })
        .collect::<Vec<_>>();

    bot.send_message(chat_id, text)
        .reply_markup(InlineKeyboardMarkup::new(rows))
        .await?;
    Ok(())
}

async fn resolve_session_id(
    state: &AppState,
    chat_id: i64,
    args: &[&str],
) -> Result<Option<String>> {
    if let Some(session_id) = args.first() {
        return Ok(Some((*session_id).to_string()));
    }
    state.db.active_session(chat_id).await
}

async fn send(bot: &Bot, chat_id: ChatId, text: impl Into<String>) -> Result<()> {
    bot.send_message(chat_id, text.into()).await?;
    Ok(())
}

fn callback_chat_id(callback: &CallbackQuery) -> i64 {
    match &callback.message {
        Some(MaybeInaccessibleMessage::Regular(message)) => message.chat.id.0,
        _ => callback.from.id.0 as i64,
    }
}

fn parse_command(text: &str) -> Option<(&str, Vec<&str>)> {
    let rest = text.trim().strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let command = parts.next()?.split('@').next().unwrap_or("");
    Some((command, parts.collect()))
}
