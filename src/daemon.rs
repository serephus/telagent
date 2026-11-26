use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

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

pub async fn reconcile_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        reconcile_pending_requests(&state).await;
        reconcile_pending_notifications(&state).await;
    }
}

async fn reconcile_pending_notifications(state: &AppState) {
    let sessions = match state.db.pending_notification_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, "failed to read pending notifications");
            return;
        }
    };
    for session in sessions {
        if session.state == "busy" || session.state == "retry" {
            continue;
        }
        if session.state == "deleted" {
            let _ = state
                .db
                .set_notification_pending(&session.session_id, false)
                .await;
            continue;
        }
        if let Err(error) = notify_completion(state, &session.session_id).await {
            tracing::warn!(
                %error,
                session_id = %session.session_id,
                "failed to retry pending completion notification"
            );
        }
    }
}

async fn reconcile_pending_requests(state: &AppState) {
    let requests = match state.db.list_pending_requests().await {
        Ok(requests) => requests,
        Err(error) => {
            tracing::warn!(%error, "failed to read pending requests");
            return;
        }
    };

    let mut checked: HashMap<(String, String), Option<HashSet<String>>> = HashMap::new();
    let mut stale: HashSet<String> = HashSet::new();
    for request in requests {
        if request.kind != "permission" && request.kind != "question" {
            continue;
        }

        let cache_key = (request.kind.clone(), request.session_id.clone());
        let ids = if let Some(ids) = checked.get(&cache_key) {
            ids.clone()
        } else {
            let directory = match state.session_directory(&request.session_id).await {
                Ok(directory) => directory,
                Err(error) => {
                    tracing::warn!(%error, session_id = %request.session_id, "failed to read session directory");
                    continue;
                }
            };
            let ids = match request.kind.as_str() {
                "permission" => {
                    state
                        .backend
                        .pending_permission_ids(&request.session_id, &directory)
                        .await
                }
                _ => {
                    state
                        .backend
                        .pending_question_ids(&request.session_id, &directory)
                        .await
                }
            };
            let ids = match ids {
                Ok(ids) => ids,
                Err(error) => {
                    tracing::warn!(%error, session_id = %request.session_id, kind = %request.kind, "failed to list pending requests");
                    continue;
                }
            };
            checked.insert(cache_key, ids.clone());
            ids
        };

        if let Some(ids) = ids
            && !ids.contains(&request.request_id)
            && stale.insert(request.request_id.clone())
            && let Err(error) = state.forget_pending_request(&request.request_id).await
        {
            tracing::warn!(%error, request_id = %request.request_id, kind = %request.kind, "failed to remove stale request");
        }
    }
}

pub async fn outbox_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        let items = match state.db.list_due_outbox(20).await {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(%error, "failed to read outbox");
                continue;
            }
        };

        for item in items {
            match state
                .db
                .get_session(&state.instance_id, &item.session_id)
                .await
            {
                Ok(Some(session)) if session.state == "deleted" => {
                    tracing::info!(session_id = %item.session_id, "dropping outbox item for deleted session");
                    let _ = state.db.remove_outbox(item.id).await;
                    continue;
                }
                Ok(None) => {
                    tracing::info!(session_id = %item.session_id, "dropping outbox item for unknown session");
                    let _ = state.db.remove_outbox(item.id).await;
                    continue;
                }
                Ok(Some(_)) => {}
                Err(error) => {
                    tracing::warn!(%error, session_id = %item.session_id, "failed to read outbox session");
                    continue;
                }
            }

            let directory = match state.session_directory(&item.session_id).await {
                Ok(directory) => directory,
                Err(error) => {
                    tracing::warn!(%error, session_id = %item.session_id, "failed to read session directory");
                    continue;
                }
            };
            match state
                .backend
                .prompt_async(&item.session_id, &directory, &item.text)
                .await
            {
                Ok(()) => {
                    if let Err(error) = state.db.remove_outbox(item.id).await {
                        tracing::warn!(%error, "failed to remove outbox item");
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    tracing::warn!(
                        %error,
                        session_id = %item.session_id,
                        attempts = item.attempts + 1,
                        "outbox delivery failed"
                    );
                    if let Err(error) = state.db.increment_outbox_attempts(item.id, &message).await
                    {
                        tracing::warn!(%error, "failed to reschedule outbox item");
                    }
                }
            }
        }
    }
}

pub async fn notification_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        if let Err(error) = state.notifier.retry_notifications(50).await {
            tracing::warn!(%error, "failed to retry notifications");
        }
    }
}

pub async fn health_loop(state: AppState) {
    let interval_secs = state.config.opencode.health_interval_secs.max(1);
    let timeout_secs = state.config.opencode.health_timeout_secs.max(1);
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    let mut healthy: Option<bool> = None;

    loop {
        interval.tick().await;
        let check =
            tokio::time::timeout(Duration::from_secs(timeout_secs), state.backend.health()).await;
        let (now_healthy, detail) = match check {
            Ok(Ok(())) => (true, None),
            Ok(Err(error)) => (false, Some(error.to_string())),
            Err(_) => (
                false,
                Some(format!("health check timed out after {timeout_secs}s")),
            ),
        };

        if !now_healthy && let Some(detail) = &detail {
            tracing::warn!(%detail, "opencode health check failed");
        }

        let notify = match healthy {
            None => !now_healthy,
            Some(previous) => previous != now_healthy,
        };
        if notify {
            send_health_notification(&state, now_healthy, detail.as_deref()).await;
        }
        healthy = Some(now_healthy);
    }
}

async fn send_health_notification(state: &AppState, healthy: bool, detail: Option<&str>) {
    let mut text = if healthy {
        "opencode connection recovered.".to_string()
    } else {
        "opencode is unreachable.".to_string()
    };
    if let Some(detail) = detail {
        text.push('\n');
        text.push_str(detail);
    }
    if let Err(error) = state.notifier.notify_system(&text).await {
        tracing::warn!(%error, "failed to send health notification");
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
            state
                .db
                .set_notification_pending(&session_id, false)
                .await?;
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
            if let Err(error) = state.forget_pending_request(&request_id).await {
                tracing::warn!(%error, request_id, "failed to remove replied permission request");
            }
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
        state.db.set_notification_pending(session_id, false).await?;
        return Ok(());
    }

    let mut session = match state.db.get_session(&state.instance_id, session_id).await? {
        Some(session) => session,
        None => return Ok(()),
    };

    if session.muted || session.state == "error" {
        state.db.set_notification_pending(session_id, false).await?;
        return Ok(());
    }

    if (session.title.is_empty() || session.directory.is_empty())
        && let Ok(sessions) = state.backend.list_sessions().await
        && let Some(info) = sessions.iter().find(|info| info.id == session_id)
    {
        session = state.sync_session(info, "idle").await?;
    }

    if session.parent_id.is_some() && !state.config.notify.child_sessions {
        state.db.set_notification_pending(session_id, false).await?;
        return Ok(());
    }

    state.db.set_notification_pending(session_id, true).await?;
    state.db.update_state(session_id, "idle").await?;
    session.state = "idle".to_string();

    let messages = state
        .backend
        .messages(session_id, &session.directory, 10)
        .await?;
    let Some((message_id, text)) = render::extract_last_assistant_text(&messages) else {
        state.db.set_notification_pending(session_id, false).await?;
        return Ok(());
    };

    if session.last_notified_message_id.as_deref() == Some(message_id.as_str()) {
        state.db.set_notification_pending(session_id, false).await?;
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
    let context = match state
        .backend
        .session_context(session_id, &session.directory)
        .await
    {
        Ok(context) => context,
        Err(error) => {
            tracing::debug!(%error, "failed to read session context");
            None
        }
    };
    let (_, sent) = state
        .notifier
        .notify_completion(&session, &message_id, body, context.as_ref())
        .await?;
    state.db.set_notification_pending(session_id, false).await?;
    if !sent.is_empty() {
        state
            .db
            .set_last_notified_message(session_id, &message_id)
            .await?;
        session.last_notified_message_id = Some(message_id);
    }

    Ok(())
}
