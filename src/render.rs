use crate::{
    opencode::types::{MessageWithParts, PermissionRequest, QuestionRequest},
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

pub fn completion_message(session: &SessionRecord, body: Option<&str>) -> String {
    let mut message = format!(
        "Session finished\nTitle: {}\nDirectory: {}",
        session_title(session),
        session.directory
    );
    if let Some(body) = body {
        message.push_str("\n\n");
        message.push_str(body.trim());
    }
    message
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

pub fn sessions_message(sessions: &[SessionRecord]) -> String {
    if sessions.is_empty() {
        return "No sessions found.".to_string();
    }
    let mut message = String::from("Recent sessions:");
    for session in sessions {
        message.push_str(&format!(
            "\n- {}  [{}]  {}",
            session_title(session),
            session.state,
            session.session_id
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
}
