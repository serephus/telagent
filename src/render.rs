use crate::{
    opencode::types::{MessageWithParts, PermissionRequest, QuestionRequest, SessionContext},
    state::SessionRecord,
};

pub fn extract_last_assistant_text(messages: &[MessageWithParts]) -> Option<(String, String)> {
    let mut assistants: Vec<&MessageWithParts> = messages
        .iter()
        .filter(|message| message.is_assistant())
        .collect();
    let completed: Vec<&MessageWithParts> = assistants
        .iter()
        .copied()
        .filter(|message| message.is_completed())
        .collect();
    if !completed.is_empty() {
        assistants = completed;
    }
    assistants.sort_by_key(|message| message.info.time.created);

    assistants.into_iter().rev().find_map(|message| {
        let text = message.text();
        if text.trim().is_empty() {
            None
        } else {
            Some((message.info.id.clone(), text))
        }
    })
}

pub fn session_title(session: &SessionRecord) -> &str {
    if session.title.trim().is_empty() {
        &session.session_id
    } else {
        &session.title
    }
}

pub fn history_message(session: &SessionRecord, messages: &[MessageWithParts]) -> String {
    let mut text = format!(
        "# {}\n\nSession: {}\nDirectory: {}\n",
        session_title(session),
        session.session_id,
        session.directory
    );
    for message in messages {
        let body = message.text();
        if body.trim().is_empty() {
            continue;
        }
        text.push_str("\n---\n\n## ");
        text.push_str(if message.is_assistant() {
            "Assistant"
        } else {
            "User"
        });
        text.push_str("\n\n");
        text.push_str(body.trim());
        text.push('\n');
    }
    text
}

pub fn completion_message(
    session: &SessionRecord,
    body: Option<&str>,
    context: Option<&SessionContext>,
) -> String {
    let mut message = format!(
        "Session finished\nTitle: {}\nDirectory: {}",
        session_title(session),
        session.directory
    );
    if let Some(context) = context {
        if let Some(model) = &context.model {
            message.push_str(&format!("\nModel: {model}"));
        }
        if let (Some(used), Some(limit)) = (context.used_tokens, context.context_limit)
            && limit > 0
        {
            let percent = (used as f64 / limit as f64 * 100.0).round() as u64;
            message.push_str(&format!(
                "\nContext: {percent}% ({}/{})",
                compact_tokens(used),
                compact_tokens(limit)
            ));
        }
    }
    if let Some(body) = body {
        message.push_str("\n\n");
        message.push_str(body.trim());
    }
    message
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub fn error_message(session: &SessionRecord, error: &str) -> String {
    format!(
        "Session error\nTitle: {}\nDirectory: {}\n\n{}",
        session_title(session),
        session.directory,
        error.trim()
    )
}

pub fn permission_message(session: &SessionRecord, request: &PermissionRequest) -> String {
    let mut message = format!(
        "Permission requested\nTitle: {}\nPermission: {}",
        session_title(session),
        request.permission
    );
    if !request.patterns.is_empty() {
        message.push_str("\nPatterns: ");
        message.push_str(&request.patterns.join(", "));
    }
    if !request.metadata.is_null() {
        message.push_str("\nDetails: ");
        message.push_str(&truncate(&request.metadata.to_string(), 1500));
    }
    message
}

pub fn question_message(session: &SessionRecord, request: &QuestionRequest) -> String {
    let mut message = format!("Question\nTitle: {}", session_title(session));
    for (index, question) in request.questions.iter().enumerate() {
        message.push_str(&format!("\n\n{}. {}", index + 1, question.question));
        if !question.header.is_empty() {
            message.push_str(&format!("\n   [{}]", question.header));
        }
        for (option_index, option) in question.options.iter().enumerate() {
            message.push_str(&format!("\n   {}. {}", option_index + 1, option.label));
            if let Some(description) = &option.description
                && !description.trim().is_empty()
            {
                message.push_str(&format!(" - {}", truncate(description, 200)));
            }
        }
    }
    message
}

pub fn sessions_message(sessions: &[SessionRecord], active_session: Option<&str>) -> String {
    if sessions.is_empty() {
        return "No sessions found.".to_string();
    }
    let mut message = String::from("Recent sessions:");
    for session in sessions {
        let marker = if active_session == Some(session.session_id.as_str()) {
            " *"
        } else {
            ""
        };
        message.push_str(&format!(
            "\n- {}  [{}]  {}{}",
            session_title(session),
            session.state,
            session.session_id,
            marker
        ));
    }
    message
}

pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(64);
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return vec![text.to_string()];
    }

    let prefix_reserve = 16;
    let body_limit = max_chars.saturating_sub(prefix_reserve).max(32);
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let mut end = (start + body_limit).min(chars.len());
        if end < chars.len() {
            let search_from = start + (body_limit * 4 / 5);
            if let Some(offset) = chars[search_from..end].iter().rposition(|ch| *ch == '\n') {
                end = search_from + offset + 1;
            }
        }
        let chunk: String = chars[start..end].iter().collect();
        chunks.push(chunk);
        start = end;
    }

    if chunks.len() <= 1 {
        return chunks;
    }

    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| format!("[{}/{}]\n{}", index + 1, total, chunk))
        .collect()
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut result: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_not_split() {
        assert_eq!(chunk_text("hello", 100), vec!["hello"]);
    }

    #[test]
    fn long_text_is_split_with_prefixes() {
        let text = "a".repeat(500);
        let chunks = chunk_text(&text, 120);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 120);
        }
        assert!(chunks[0].starts_with("[1/"));
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("héllo", 3), "hél...");
    }

    #[test]
    fn completion_message_includes_context() {
        let session = SessionRecord {
            instance_id: "instance".to_string(),
            session_id: "ses_1".to_string(),
            title: "Test".to_string(),
            directory: "/tmp".to_string(),
            project_id: None,
            parent_id: None,
            state: "idle".to_string(),
            last_assistant_message_id: None,
            last_notified_message_id: None,
            muted: false,
            updated_at: 0,
        };
        let context = SessionContext {
            model: Some("gpt-5".to_string()),
            used_tokens: Some(750_000),
            context_limit: Some(1_000_000),
        };

        let message = completion_message(&session, None, Some(&context));
        assert!(message.contains("Model: gpt-5"));
        assert!(message.contains("Context: 75% (750k/1.0M)"));
    }

    #[test]
    fn history_message_includes_roles_and_text() {
        let session = SessionRecord {
            instance_id: "instance".to_string(),
            session_id: "ses_1".to_string(),
            title: "Test".to_string(),
            directory: "/tmp".to_string(),
            project_id: None,
            parent_id: None,
            state: "idle".to_string(),
            last_assistant_message_id: None,
            last_notified_message_id: None,
            muted: false,
            updated_at: 0,
        };
        let messages = vec![
            MessageWithParts {
                info: crate::opencode::types::MessageInfo {
                    id: "msg_1".to_string(),
                    role: "user".to_string(),
                    time: Default::default(),
                    tokens: Default::default(),
                },
                parts: vec![crate::opencode::types::Part {
                    kind: "text".to_string(),
                    text: Some("hello".to_string()),
                }],
            },
            MessageWithParts {
                info: crate::opencode::types::MessageInfo {
                    id: "msg_2".to_string(),
                    role: "assistant".to_string(),
                    time: Default::default(),
                    tokens: Default::default(),
                },
                parts: vec![crate::opencode::types::Part {
                    kind: "text".to_string(),
                    text: Some("hi there".to_string()),
                }],
            },
        ];

        let history = history_message(&session, &messages);
        assert!(history.contains("# Test"));
        assert!(history.contains("## User\n\nhello"));
        assert!(history.contains("## Assistant\n\nhi there"));
    }
}
