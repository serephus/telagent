use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;

use crate::{
    app::AppState,
    opencode::{
        AgentEvent,
        types::{SessionInfo, SessionStatus},
    },
    render,
};

pub async fn event_loop(state: AppState) {
    let mut backoff = 1u64;
    loop {
        match state.backend.event_stream().await {
            Ok(mut stream) => {
                backoff = 1;
                if let Err(error) = resync(&state).await {
                    tracing::warn!(%error, "failed to resync opencode state");
                }

                while let Some(item) = stream.next().await {
                    match item {
                        Ok(raw) => {
                            let event = state.backend.normalize_event(raw);
                            if let Err(error) = handle_event(&state, event).await {
                                tracing::warn!(%error, "failed to handle opencode event");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "opencode event stream ended");
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to connect to opencode event stream");
            }
        }

        tracing::info!(backoff, "reconnecting to opencode event stream");
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

pub async fn outbox_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        let items = match state.db.list_outbox(20).await {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(%error, "failed to read outbox");
                continue;
            }
        };

        for item in items {
            match state
                .backend
                .prompt_async(&item.session_id, &item.text)
                .await
            {
                Ok(()) => {
                    if let Err(error) = state.db.remove_outbox(item.id).await {
                        tracing::warn!(%error, "failed to remove outbox item");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, session_id = %item.session_id, "outbox delivery failed");
                    let _ = state.db.increment_outbox_attempts(item.id).await;
                    if item.attempts + 1 >= 5 {
                        let _ = state.db.remove_outbox(item.id).await;
                    }
                }
            }
        }
    }
}

async fn resync(state: &AppState) -> Result<()> {
    let statuses = state.backend.session_statuses().await?;
    let sessions = state.backend.list_sessions().await?;

    for info in sessions {
        let new_state = statuses
            .get(&info.id)
            .map(SessionStatus::as_db_state)
            .unwrap_or("idle");
        let previous = state.db.get_session(&state.instance_id, &info.id).await?;
        let session = state.sync_session(&info, new_state).await?;

        if previous
            .as_ref()
            .map(|session| session.state == "busy" || session.state == "retry")
            .unwrap_or(false)
            && new_state == "idle"
            && let Err(error) = notify_completion(state, &session.session_id).await
        {
            tracing::warn!(%error, "failed to notify completion during resync");
        }
    }

    for session_id in statuses.keys() {
        if state
            .db
            .get_session(&state.instance_id, session_id)
            .await?
            .is_none()
        {
            let info = SessionInfo {
                id: session_id.clone(),
                ..Default::default()
            };
            let status = statuses.get(session_id).unwrap();
            state.sync_session(&info, status.as_db_state()).await?;
        }
    }

    Ok(())
}

async fn handle_event(state: &AppState, event: AgentEvent) -> Result<()> {
    tracing::trace!(kind = event.kind(), "handling opencode event");
    match event {
        AgentEvent::SessionCreated(info) => {
            let session = state.sync_session(&info, "idle").await?;
            if state.config.notify.on_created
                && (session.parent_id.is_none() || state.config.notify.child_sessions)
            {
                state.notifier.notify_created(&session).await?;
            }
        }
        AgentEvent::SessionUpdated(info) => {
            let existing_state = state
                .db
                .get_session(&state.instance_id, &info.id)
                .await?
                .map(|session| session.state)
                .unwrap_or_else(|| "idle".to_string());
            state.sync_session(&info, &existing_state).await?;
        }
        AgentEvent::SessionDeleted(session_id) => {
            state.db.update_state(&session_id, "deleted").await?;
        }
        AgentEvent::SessionStatus { session_id, status } => {
            let info = SessionInfo {
                id: session_id.clone(),
                ..Default::default()
            };
            state.sync_session(&info, status.as_db_state()).await?;
            if let Some(message) = status.retry_message() {
                tracing::warn!(session_id = %session_id, message, "opencode is retrying");
            }
        }
        AgentEvent::SessionIdle(session_id) => {
            if let Err(error) = notify_completion(state, &session_id).await {
                tracing::warn!(%error, "failed to notify session idle");
            }
        }
        AgentEvent::SessionError {
            session_id,
            message,
        } => {
            let session = state.get_or_placeholder_session(&session_id).await?;
            state.db.update_state(&session_id, "error").await?;
            if state.config.notify.on_error
                && (session.parent_id.is_none() || state.config.notify.child_sessions)
            {
                state.notifier.notify_error(&session, &message).await?;
            }
        }
        AgentEvent::PermissionAsked(request) => {
            if state.config.notify.permissions {
                let session = state
                    .get_or_placeholder_session(&request.session_id)
                    .await?;
                state.notifier.notify_permission(&session, &request).await?;
            }
        }
        AgentEvent::QuestionAsked(request) => {
            if state.config.notify.questions {
                let session = state
                    .get_or_placeholder_session(&request.session_id)
                    .await?;
                state.notifier.notify_question(&session, &request).await?;
            }
        }
        AgentEvent::PermissionReplied {
            session_id,
            request_id,
            reply,
        } => {
            tracing::debug!(session_id, request_id, reply, "permission replied");
            let _ = state.db.remove_pending_request(&request_id).await;
        }
        AgentEvent::QuestionReplied {
            session_id,
            request_id,
        } => {
            tracing::debug!(?session_id, request_id, "question replied");
            let _ = state.db.remove_pending_request(&request_id).await;
        }
        AgentEvent::Other(kind) => {
            tracing::trace!(kind, "ignoring opencode event");
        }
    }
    Ok(())
}

async fn notify_completion(state: &AppState, session_id: &str) -> Result<()> {
    if !state.config.notify.on_idle {
        return Ok(());
    }

    let mut session = match state.db.get_session(&state.instance_id, session_id).await? {
        Some(session) => session,
        None => return Ok(()),
    };

    if session.muted {
        return Ok(());
    }
    if session.state == "error" {
        return Ok(());
    }

    if (session.title.is_empty() || session.directory.is_empty())
        && let Ok(sessions) = state.backend.list_sessions().await
        && let Some(info) = sessions.iter().find(|info| info.id == session_id)
    {
        session = state.sync_session(info, "idle").await?;
    }

    if session.parent_id.is_some() && !state.config.notify.child_sessions {
        return Ok(());
    }

    state.db.update_state(session_id, "idle").await?;
    session.state = "idle".to_string();

    let messages = state.backend.messages(session_id, 10).await?;
    let Some((message_id, text)) = render::extract_last_assistant_text(&messages) else {
        return Ok(());
    };

    if session.last_notified_message_id.as_deref() == Some(message_id.as_str()) {
        return Ok(());
    }

    state
        .db
        .set_last_assistant_message(session_id, &message_id)
        .await?;
    session.last_assistant_message_id = Some(message_id.clone());

    let body = if state.config.notify.include_last_message {
        Some(text.as_str())
    } else {
        None
    };
    let (_, sent) = state.notifier.notify_completion(&session, body).await?;
    if !sent.is_empty() {
        state
            .db
            .set_last_notified_message(session_id, &message_id)
            .await?;
        session.last_notified_message_id = Some(message_id);
    }

    Ok(())
}
