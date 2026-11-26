use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    prelude::*,
    types::{
        CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, LinkPreviewOptions,
        MaybeInaccessibleMessage, Message, Update, UpdateKind,
    },
};

use crate::{
    app::AppState,
    opencode::types::{PermissionReply, PermissionRequest, QuestionRequest},
    pairing, render,
};

pub type HandlerResult = ResponseResult<()>;

struct ParsedCommand<'a> {
    command: &'a str,
    args: Vec<&'a str>,
    raw_args: &'a str,
}

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
        && let Some(parsed) = parse_command(text)
    {
        let reply_session_id = match message.reply_to_message() {
            Some(reply) => {
                state
                    .db
                    .session_for_telegram_message(chat_id.0, reply.id.0)
                    .await?
            }
            None => None,
        };
        handle_command(
            bot,
            state,
            message,
            &parsed,
            bound,
            reply_session_id.as_deref(),
        )
        .await?;
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
    parsed: &ParsedCommand<'_>,
    bound: bool,
    reply_session_id: Option<&str>,
) -> Result<()> {
    let chat_id = message.chat.id;
    let user_id = message.from.as_ref().map(|user| user.id.0).unwrap_or(0);
    let command = parsed.command;
    let args = &parsed.args;

    match command {
        "start" | "help" => {
            send(bot, chat_id, crate::telegram::commands::help_text()).await?;
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
        "new" => {
            let (requested_directory, prompt) = parse_new_args(parsed.raw_args);
            let requested = requested_directory.as_deref().unwrap_or("");
            let active_directory = match state.db.active_session(chat_id.0).await? {
                Some(session_id) => state.session_directory(&session_id).await?,
                None => String::new(),
            };
            let configured_directory = state
                .config
                .opencode
                .directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            let directory = if requested.is_empty() {
                if !active_directory.is_empty() {
                    PathBuf::from(active_directory)
                } else if !configured_directory.is_empty() {
                    PathBuf::from(configured_directory)
                } else {
                    PathBuf::new()
                }
            } else {
                let base = if !active_directory.is_empty() {
                    active_directory
                } else if !configured_directory.is_empty() {
                    configured_directory
                } else {
                    std::env::current_dir()?.to_string_lossy().into_owned()
                };
                resolve_directory(requested, &base)
            };
            let directory = if directory.as_os_str().is_empty() || directory.is_absolute() {
                directory
            } else {
                std::env::current_dir()?.join(directory)
            };
            let directory = directory.to_string_lossy().into_owned();
            if !directory.is_empty()
                && let Err(error) = std::fs::create_dir_all(&directory)
            {
                tracing::warn!(directory = %directory, %error, "failed to create session directory");
                send(
                    bot,
                    chat_id,
                    format!("Failed to create directory {directory}: {error}"),
                )
                .await?;
                return Ok(());
            }
            let directory = if directory.is_empty() {
                directory
            } else {
                match std::fs::canonicalize(&directory) {
                    Ok(path) => path.to_string_lossy().into_owned(),
                    Err(error) => {
                        tracing::warn!(directory = %directory, %error, "failed to resolve session directory");
                        send(
                            bot,
                            chat_id,
                            format!("Failed to resolve directory {directory}: {error}"),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            };
            match state.backend.create_session(&directory).await {
                Ok(info) => {
                    let session = state.sync_session(&info, "idle").await?;
                    let session_directory = state.session_directory(&session.session_id).await?;
                    state
                        .db
                        .set_active_session(chat_id.0, &session.session_id)
                        .await?;
                    let mut text = format!(
                        "Started session {}\n{}",
                        session.session_id,
                        if session.title.is_empty() {
                            session.directory.clone()
                        } else {
                            session.title.clone()
                        }
                    );
                    if let Some(prompt) = prompt {
                        match state
                            .backend
                            .prompt_async(&session.session_id, &session_directory, &prompt)
                            .await
                        {
                            Ok(()) => {
                                text.push_str("\nPrompt sent.");
                            }
                            Err(error) => {
                                state
                                    .db
                                    .enqueue_outbox(&session.session_id, &prompt)
                                    .await?;
                                text.push_str(&format!("\nPrompt queued: {error}"));
                            }
                        }
                    }
                    send(bot, chat_id, text).await?;
                }
                Err(error) => {
                    tracing::warn!(directory = %directory, %error, "failed to create session");
                    send(
                        bot,
                        chat_id,
                        format!("Failed to start session in {directory}: {error}"),
                    )
                    .await?;
                }
            }
        }
        "compact" => {
            let Some(session_id) =
                resolve_session_id(state, chat_id.0, args, reply_session_id).await?
            else {
                send(bot, chat_id, "No active session. Pass a session ID.").await?;
                return Ok(());
            };
            send(bot, chat_id, format!("Compacting {session_id}...")).await?;
            match state
                .backend
                .compact_session(&session_id, &state.session_directory(&session_id).await?)
                .await
            {
                Ok(()) => {
                    send(bot, chat_id, format!("Compacted {session_id}.")).await?;
                }
                Err(error) => {
                    send(bot, chat_id, format!("Compaction failed: {error}")).await?;
                }
            }
        }
        "sessions" => {
            send_sessions(bot, state, chat_id).await?;
        }
        "status" => {
            let sessions = state.db.list_sessions(&state.instance_id, 10).await?;
            let active_session = state.db.active_session(chat_id.0).await?;
            let pending = state.db.pending_request_count(chat_id.0).await?;
            let mut text = render::sessions_message(&sessions, active_session.as_deref());
            if let Some(active_session) = &active_session {
                text.push_str(&format!("\n\nActive session: {active_session}"));
            }
            if pending > 0 {
                text.push_str(&format!("\nPending requests: {pending}"));
            }
            send(bot, chat_id, text).await?;
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
        "cancel" => match state.db.active_session(chat_id.0).await? {
            Some(session_id) => {
                state.db.clear_active_session(chat_id.0).await?;
                send(
                    bot,
                    chat_id,
                    format!("Cleared active session {session_id}."),
                )
                .await?;
            }
            None => {
                send(bot, chat_id, "No active session.").await?;
            }
        },
        "mute" | "unmute" => {
            let Some(session_id) =
                resolve_session_id(state, chat_id.0, args, reply_session_id).await?
            else {
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
            let Some(session_id) =
                resolve_session_id(state, chat_id.0, args, reply_session_id).await?
            else {
                send(bot, chat_id, "No active session. Pass a session ID.").await?;
                return Ok(());
            };
            match state
                .backend
                .abort(&session_id, &state.session_directory(&session_id).await?)
                .await
            {
                Ok(()) => {
                    send(bot, chat_id, format!("Aborted {session_id}.")).await?;
                }
                Err(error) => {
                    send(bot, chat_id, format!("Abort failed: {error}")).await?;
                }
            }
        }
        "export" => {
            export_history(bot, state, chat_id, parsed.raw_args, reply_session_id).await?;
        }
        other => {
            send(bot, chat_id, format!("Unknown command: /{other}")).await?;
        }
    }

    Ok(())
}

async fn export_history(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    raw_args: &str,
    reply_session_id: Option<&str>,
) -> Result<()> {
    let options = match parse_export_args(raw_args) {
        Ok(options) => options,
        Err(error) => {
            send(
                bot,
                chat_id,
                format!("Usage: /export [ID] [--title TITLE] [--reveal TIME]\n{error}"),
            )
            .await?;
            return Ok(());
        }
    };

    let session_id = match options.session_id {
        Some(session_id) => session_id,
        None => match reply_session_id {
            Some(session_id) => session_id.to_string(),
            None => match state.db.active_session(chat_id.0).await? {
                Some(session_id) => session_id,
                None => {
                    send(bot, chat_id, "No active session. Pass a session ID.").await?;
                    return Ok(());
                }
            },
        },
    };

    send(bot, chat_id, format!("Exporting {session_id} history...")).await?;

    let directory = state.session_directory(&session_id).await?;
    let session = state.get_or_placeholder_session(&session_id).await?;
    let messages = match state.backend.history(&session_id, &directory).await {
        Ok(messages) => messages,
        Err(error) => {
            send(bot, chat_id, format!("Export failed: {error}")).await?;
            return Ok(());
        }
    };
    if messages.is_empty() {
        send(bot, chat_id, "No history to export.").await?;
        return Ok(());
    }

    let text = render::history_message(&session, &messages);
    let title = options
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| render::session_title(&session).to_string());
    let reveal = options
        .reveal
        .as_deref()
        .filter(|reveal| !reveal.eq_ignore_ascii_case("now"));

    match state.paste.upload(&text, &title, reveal).await {
        Ok(url) => {
            send(
                bot,
                chat_id,
                format!("Exported {session_id} history to {url}"),
            )
            .await?;
        }
        Err(error) => {
            send(bot, chat_id, format!("Export failed: {error}")).await?;
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
                .reply_permission(
                    &pending.session_id,
                    &state.session_directory(&pending.session_id).await?,
                    request_id,
                    reply,
                )
                .await
            {
                Ok(()) => {
                    let request = pending.payload.as_deref().and_then(|payload| {
                        serde_json::from_str::<PermissionRequest>(payload).ok()
                    });
                    let mut text = format!(
                        "Permission response sent: {}\nSession: {}",
                        reply.as_str(),
                        pending.session_id
                    );
                    if let Some(request) = request {
                        text.push_str(&format!("\nPermission: {}", request.permission));
                        if !request.patterns.is_empty() {
                            text.push_str(&format!(
                                "\nPatterns: {}",
                                render::truncate(&request.patterns.join(", "), 1500)
                            ));
                        }
                    }
                    send(bot, ChatId(chat_id), text).await?;
                    if let Err(error) = state.forget_pending_request(request_id).await {
                        tracing::warn!(%error, request_id, "failed to remove permission request");
                    }
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
                .reply_question(
                    &pending.session_id,
                    &state.session_directory(&pending.session_id).await?,
                    request_id,
                    &request,
                    answers,
                )
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
    let directory = state.session_directory(session_id).await?;
    match state
        .backend
        .prompt_async(session_id, &directory, text)
        .await
    {
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
        .reply_question(
            &pending.session_id,
            &state.session_directory(&pending.session_id).await?,
            &pending.request_id,
            &request,
            answers,
        )
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
    let active_session = state.db.active_session(chat_id.0).await?;
    let text = render::sessions_message(&sessions, active_session.as_deref());
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
        .link_preview_options(LinkPreviewOptions {
            is_disabled: true,
            url: None,
            prefer_small_media: false,
            prefer_large_media: false,
            show_above_text: false,
        })
        .await?;
    Ok(())
}

async fn resolve_session_id(
    state: &AppState,
    chat_id: i64,
    args: &[&str],
    reply_session_id: Option<&str>,
) -> Result<Option<String>> {
    if let Some(session_id) = args.first() {
        return Ok(Some((*session_id).to_string()));
    }
    if let Some(session_id) = reply_session_id {
        return Ok(Some(session_id.to_string()));
    }
    state.db.active_session(chat_id).await
}

fn resolve_directory(requested: &str, base: &str) -> PathBuf {
    let expanded = if requested == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(requested))
    } else if let Some(rest) = requested.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(requested))
    } else {
        PathBuf::from(requested)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        PathBuf::from(base).join(expanded)
    }
}

async fn send(bot: &Bot, chat_id: ChatId, text: impl Into<String>) -> Result<()> {
    bot.send_message(chat_id, text.into())
        .link_preview_options(LinkPreviewOptions {
            is_disabled: true,
            url: None,
            prefer_small_media: false,
            prefer_large_media: false,
            show_above_text: false,
        })
        .await?;
    Ok(())
}

fn callback_chat_id(callback: &CallbackQuery) -> i64 {
    match &callback.message {
        Some(MaybeInaccessibleMessage::Regular(message)) => message.chat.id.0,
        _ => callback.from.id.0 as i64,
    }
}

fn parse_command(text: &str) -> Option<ParsedCommand<'_>> {
    let rest = text.trim().strip_prefix('/')?;
    let (head, raw_args) = rest
        .split_once(|ch: char| ch.is_whitespace())
        .unwrap_or((rest, ""));
    let command = head.split('@').next().unwrap_or("");
    let raw_args = raw_args.trim();
    Some(ParsedCommand {
        command,
        args: raw_args.split_whitespace().collect(),
        raw_args,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ExportArgs {
    session_id: Option<String>,
    title: Option<String>,
    reveal: Option<String>,
}

fn parse_export_args(input: &str) -> std::result::Result<ExportArgs, String> {
    let mut options = ExportArgs::default();
    let mut tokens = split_args(input).into_iter();
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--title" => match tokens.next() {
                Some(value) => options.title = Some(value),
                None => return Err("--title requires a value".to_string()),
            },
            "--reveal" => match tokens.next() {
                Some(value) => options.reveal = Some(value),
                None => return Err("--reveal requires a value".to_string()),
            },
            value if value.starts_with("--") => {
                return Err(format!("unknown option {value}"));
            }
            value if options.session_id.is_none() => {
                options.session_id = Some(value.to_string());
            }
            value => return Err(format!("unexpected argument {value}")),
        }
    }
    Ok(options)
}

fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;

    for ch in input.chars() {
        match quote {
            Some(expected) => {
                if ch == expected {
                    quote = None;
                } else {
                    current.push(ch);
                }
                started = true;
            }
            None => match ch {
                '"' | '\'' => {
                    quote = Some(ch);
                    started = true;
                }
                ch if ch.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                ch => {
                    current.push(ch);
                    started = true;
                }
            },
        }
    }
    if started {
        args.push(current);
    }
    args
}

fn parse_new_args(input: &str) -> (Option<String>, Option<String>) {
    let input = input.trim();
    if input.is_empty() {
        return (None, None);
    }
    if let Some(prompt) = input.strip_prefix("--") {
        let prompt = prompt.trim();
        return (None, (!prompt.is_empty()).then(|| prompt.to_string()));
    }
    let (directory, rest) = parse_first_argument(input);
    let rest = rest.trim();
    let rest = rest.strip_prefix("--").map(str::trim).unwrap_or(rest);
    let prompt = (!rest.is_empty()).then(|| rest.to_string());
    (Some(directory), prompt)
}

fn parse_first_argument(input: &str) -> (String, &str) {
    if let Some(rest) = input.strip_prefix('"') {
        return match rest.split_once('"') {
            Some((directory, rest)) => (directory.to_string(), rest),
            None => (rest.to_string(), ""),
        };
    }
    if let Some(rest) = input.strip_prefix('\'') {
        return match rest.split_once('\'') {
            Some((directory, rest)) => (directory.to_string(), rest),
            None => (rest.to_string(), ""),
        };
    }
    match input.split_once(|ch: char| ch.is_whitespace()) {
        Some((directory, rest)) => (directory.to_string(), rest),
        None => (input.to_string(), ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_directory_under_base() {
        assert_eq!(
            resolve_directory("project", "/tmp/base"),
            PathBuf::from("/tmp/base/project")
        );
    }

    #[test]
    fn resolves_absolute_directory_as_is() {
        assert_eq!(
            resolve_directory("/tmp/project", "/tmp/base"),
            PathBuf::from("/tmp/project")
        );
    }

    #[test]
    fn parses_new_directory_only() {
        assert_eq!(
            parse_new_args("/tmp/project"),
            (Some("/tmp/project".to_string()), None)
        );
    }

    #[test]
    fn parses_new_directory_and_prompt() {
        assert_eq!(
            parse_new_args("/tmp/project fix the build"),
            (
                Some("/tmp/project".to_string()),
                Some("fix the build".to_string())
            )
        );
    }

    #[test]
    fn parses_quoted_new_directory() {
        assert_eq!(
            parse_new_args("\"/tmp/my project\" fix the build"),
            (
                Some("/tmp/my project".to_string()),
                Some("fix the build".to_string())
            )
        );
    }

    #[test]
    fn parses_new_prompt_without_directory() {
        assert_eq!(
            parse_new_args("-- fix the build"),
            (None, Some("fix the build".to_string()))
        );
    }

    #[test]
    fn parses_new_directory_separator() {
        assert_eq!(
            parse_new_args("/tmp/project -- fix the build"),
            (
                Some("/tmp/project".to_string()),
                Some("fix the build".to_string())
            )
        );
    }

    #[test]
    fn parses_export_options() {
        assert_eq!(
            parse_export_args("ses_1 --title \"Release notes\" --reveal '2030-01-01 00:00'"),
            Ok(ExportArgs {
                session_id: Some("ses_1".to_string()),
                title: Some("Release notes".to_string()),
                reveal: Some("2030-01-01 00:00".to_string()),
            })
        );
    }

    #[test]
    fn rejects_export_options_without_values() {
        assert_eq!(
            parse_export_args("--title"),
            Err("--title requires a value".to_string())
        );
        assert_eq!(
            parse_export_args("--reveal"),
            Err("--reveal requires a value".to_string())
        );
    }
}
