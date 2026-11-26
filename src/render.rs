use crate::{
    opencode::types::{MessageWithParts, Part, PermissionRequest, QuestionRequest, SessionContext},
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

const HISTORY_SEPARATOR: &str = "\n---\n\n";
const MIN_HISTORY_LIMIT: usize = 256;

#[derive(Debug, Clone)]
struct HistoryBlock {
    heading: String,
    body: String,
}

impl HistoryBlock {
    fn rendered(&self) -> String {
        format!("{}\n\n{}\n", self.heading, self.body)
    }

    fn cost(&self) -> usize {
        self.heading.len() + self.body.len() + 3 + HISTORY_SEPARATOR.len()
    }
}

pub fn history_pastes(
    session: &SessionRecord,
    messages: &[MessageWithParts],
    max_bytes: usize,
) -> Vec<String> {
    let turns = history_turns(messages);
    if turns.is_empty() {
        return Vec::new();
    }

    let header_len = history_header(session, 1, 1).len();
    let mut limit = max_bytes.saturating_sub(header_len).max(MIN_HISTORY_LIMIT);

    loop {
        let split_turns = turns
            .iter()
            .flat_map(|turn| split_turn(turn, limit))
            .collect();
        let groups = pack_turns(split_turns, limit);
        let total = groups.len();
        let pastes = groups
            .iter()
            .enumerate()
            .map(|(index, blocks)| history_paste(session, blocks, index + 1, total))
            .collect::<Vec<_>>();
        if pastes.iter().all(|paste| paste.len() <= max_bytes) || limit <= MIN_HISTORY_LIMIT {
            return pastes;
        }
        limit = (limit * 3) / 4;
    }
}

fn history_header(session: &SessionRecord, part: usize, total: usize) -> String {
    format!(
        "# {}\n\nSession: {}\nDirectory: {}\nPart: {}/{}\n",
        session_title(session),
        session.session_id,
        session.directory,
        part,
        total
    )
}

fn history_paste(
    session: &SessionRecord,
    blocks: &[HistoryBlock],
    part: usize,
    total: usize,
) -> String {
    let mut text = history_header(session, part, total);
    for block in blocks {
        text.push_str(HISTORY_SEPARATOR);
        text.push_str(&block.rendered());
    }
    text
}

fn history_turns(messages: &[MessageWithParts]) -> Vec<Vec<HistoryBlock>> {
    let mut turns = Vec::new();
    let mut current = Vec::new();
    for message in messages {
        let blocks = message_blocks(message);
        if blocks.is_empty() {
            continue;
        }
        if !message.is_assistant() && !current.is_empty() {
            turns.push(std::mem::take(&mut current));
        }
        current.extend(blocks);
    }
    if !current.is_empty() {
        turns.push(current);
    }
    turns
}

const USER_HEADING: &str = "## User";
const ASSISTANT_HEADING: &str = "## Assistant";
const REASONING_HEADING: &str = "### Reasoning";

const MAX_TOOL_INPUT_BYTES: usize = 4 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_TOOL_ERROR_BYTES: usize = 4 * 1024;
const MAX_FILE_URL_BYTES: usize = 2 * 1024;
const MAX_UNKNOWN_PART_BYTES: usize = 4 * 1024;

fn message_blocks(message: &MessageWithParts) -> Vec<HistoryBlock> {
    let text_heading = if message.is_assistant() {
        ASSISTANT_HEADING
    } else {
        USER_HEADING
    };
    let mut blocks = Vec::new();
    let mut pending: Option<(String, String)> = None;

    for part in &message.parts {
        let Some((heading, body)) = render_part(part, text_heading) else {
            continue;
        };
        let mergeable =
            heading == USER_HEADING || heading == ASSISTANT_HEADING || heading == REASONING_HEADING;
        if let Some((pending_heading, pending_body)) = pending.as_mut()
            && *pending_heading == heading
        {
            if !pending_body.is_empty() && !body.is_empty() {
                pending_body.push_str("\n\n");
            }
            pending_body.push_str(&body);
            continue;
        }
        if let Some((pending_heading, pending_body)) = pending.take() {
            blocks.push(HistoryBlock {
                heading: pending_heading,
                body: pending_body,
            });
        }
        if mergeable {
            pending = Some((heading, body));
        } else {
            blocks.push(HistoryBlock { heading, body });
        }
    }
    if let Some((heading, body)) = pending {
        blocks.push(HistoryBlock { heading, body });
    }
    blocks
}

fn render_part(part: &Part, text_heading: &str) -> Option<(String, String)> {
    match part.kind.as_str() {
        "text" => {
            if part.ignored {
                return None;
            }
            let text = part.text.as_deref()?.trim();
            if text.is_empty() {
                return None;
            }
            Some((text_heading.to_string(), text.to_string()))
        }
        "reasoning" => {
            let text = part.text.as_deref()?.trim();
            if text.is_empty() {
                return None;
            }
            Some((REASONING_HEADING.to_string(), text.to_string()))
        }
        "tool" => Some(render_tool(part)),
        "file" => Some(render_file(part)),
        "agent" => Some(render_agent(part)),
        "subtask" => Some(render_subtask(part)),
        "step-finish" => Some(render_step_finish(part)),
        "patch" => Some(render_patch(part)),
        "retry" => Some(render_retry(part)),
        "compaction" => Some(render_compaction(part)),
        "step-start" | "snapshot" => None,
        other => Some(render_unknown(other, part)),
    }
}

fn render_tool(part: &Part) -> (String, String) {
    let tool = part
        .tool
        .as_deref()
        .filter(|tool| !tool.is_empty())
        .unwrap_or("tool");
    let heading = format!("### Tool: {tool}");
    let Some(state) = part.state.as_ref() else {
        return (heading, String::new());
    };

    let status = if state.status.is_empty() {
        "unknown"
    } else {
        state.status.as_str()
    };
    let mut body = format!("Status: {status}");
    if let Some(call_id) = part
        .call_id
        .as_deref()
        .filter(|call_id| !call_id.is_empty())
    {
        body.push_str(&format!("\nCall: {call_id}"));
    }
    if let Some(title) = state.title.as_deref().filter(|title| !title.is_empty()) {
        body.push_str(&format!("\nTitle: {title}"));
    }
    if !state.input.is_null()
        && state
            .input
            .as_object()
            .is_none_or(|input| !input.is_empty())
    {
        let input = serde_json::to_string_pretty(&state.input).unwrap_or_else(|_| "{}".to_string());
        body.push_str("\nInput:\n```json\n");
        body.push_str(&truncate_bytes(&input, MAX_TOOL_INPUT_BYTES));
        body.push_str("\n```");
    }
    if let Some(output) = state
        .output
        .as_deref()
        .filter(|output| !output.trim().is_empty())
    {
        body.push_str("\nOutput:\n");
        body.push_str(&truncate_bytes(output, MAX_TOOL_OUTPUT_BYTES));
    }
    if let Some(error) = state
        .error
        .as_deref()
        .filter(|error| !error.trim().is_empty())
    {
        body.push_str("\nError:\n");
        body.push_str(&truncate_bytes(error, MAX_TOOL_ERROR_BYTES));
    }
    (heading, body)
}

fn render_file(part: &Part) -> (String, String) {
    let filename = part.filename.as_deref().filter(|name| !name.is_empty());
    let mime = part.mime.as_deref().filter(|mime| !mime.is_empty());
    let label = filename.or(mime).unwrap_or("attachment");
    let heading = format!("### File: {label}");

    let mut body = String::new();
    if filename.is_some()
        && let Some(mime) = mime
    {
        body.push_str(&format!("Type: {mime}"));
    }
    if let Some(source) = part.extra.get("source")
        && let Some(path) = source.get("path").and_then(serde_json::Value::as_str)
    {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("Source: {path}"));
    }
    if let Some(url) = part.url.as_deref().filter(|url| !url.is_empty()) {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&summarize_url(url));
    }
    (heading, body)
}

fn render_agent(part: &Part) -> (String, String) {
    let name = part
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or("agent");
    (format!("### Agent: {name}"), String::new())
}

fn render_subtask(part: &Part) -> (String, String) {
    let agent = part
        .agent
        .as_deref()
        .filter(|agent| !agent.is_empty())
        .unwrap_or("subagent");
    let heading = format!("### Subtask: {agent}");

    let mut body = String::new();
    if let Some(description) = part
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
    {
        body.push_str(&format!("Description: {description}"));
    }
    if let Some(prompt) = part
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("Prompt:\n");
        body.push_str(&truncate_bytes(prompt, MAX_TOOL_OUTPUT_BYTES));
    }
    (heading, body)
}

fn render_step_finish(part: &Part) -> (String, String) {
    let reason = part
        .reason
        .as_deref()
        .filter(|reason| !reason.is_empty())
        .unwrap_or("finished");
    let heading = format!("### Step: {reason}");

    let mut details = Vec::new();
    if let Some(tokens) = &part.tokens {
        details.push(format!("{} tokens", tokens.total()));
    }
    if let Some(cost) = part.cost {
        details.push(format!("${cost:.4}"));
    }
    (heading, details.join(" · "))
}

fn render_patch(part: &Part) -> (String, String) {
    let mut body = format!("{} file(s)", part.files.len());
    for file in &part.files {
        body.push_str(&format!("\n- {file}"));
    }
    if let Some(hash) = part.hash.as_deref().filter(|hash| !hash.is_empty()) {
        body.push_str(&format!("\nHash: {hash}"));
    }
    ("### Patch".to_string(), body)
}

fn render_retry(part: &Part) -> (String, String) {
    let attempt = part.attempt.unwrap_or(0);
    let heading = format!("### Retry #{attempt}");
    let body = part.error.as_ref().map(error_text).unwrap_or_default();
    (heading, body)
}

fn render_compaction(part: &Part) -> (String, String) {
    let mut flags = Vec::new();
    if part.auto == Some(true) {
        flags.push("auto");
    }
    if part.overflow == Some(true) {
        flags.push("overflow");
    }
    ("### Compaction".to_string(), flags.join(", "))
}

fn render_unknown(kind: &str, part: &Part) -> (String, String) {
    let mut map = part.extra.clone();
    if let Some(text) = &part.text {
        map.insert("text".to_string(), serde_json::Value::String(text.clone()));
    }
    let raw = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .unwrap_or_else(|_| "{}".to_string());
    (
        format!("### {kind}"),
        truncate_bytes(&raw, MAX_UNKNOWN_PART_BYTES),
    )
}

fn summarize_url(url: &str) -> String {
    let Some(rest) = url.strip_prefix("data:") else {
        return format!("URL: {}", truncate_bytes(url, MAX_FILE_URL_BYTES));
    };
    let header = rest.split(',').next().unwrap_or_default();
    let payload_bytes = rest
        .split_once(',')
        .map(|(_, payload)| payload.len())
        .unwrap_or(0);
    format!("URL: data:{header},… ({payload_bytes} bytes)")
}

fn error_text(error: &serde_json::Value) -> String {
    if let Some(message) = error
        .get("data")
        .and_then(|data| data.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        return truncate_bytes(message, MAX_TOOL_ERROR_BYTES);
    }
    if let Some(message) = error.get("message").and_then(serde_json::Value::as_str) {
        return truncate_bytes(message, MAX_TOOL_ERROR_BYTES);
    }
    truncate_bytes(&error.to_string(), MAX_TOOL_ERROR_BYTES)
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = text.len() - end;
    let mut result = text[..end].to_string();
    result.push_str(&format!("\n… [truncated {omitted} bytes]"));
    result
}

fn split_turn(turn: &[HistoryBlock], limit: usize) -> Vec<Vec<HistoryBlock>> {
    let mut blocks = Vec::new();
    for block in turn {
        blocks.extend(split_block(block, limit));
    }

    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut cost = 0;
    for block in blocks {
        let next = block.cost();
        if !current.is_empty() && cost + next > limit {
            groups.push(std::mem::take(&mut current));
            cost = 0;
        }
        current.push(block);
        cost += next;
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

fn split_block(block: &HistoryBlock, limit: usize) -> Vec<HistoryBlock> {
    let overhead = block.heading.len() + 3 + HISTORY_SEPARATOR.len();
    let max_body = limit.saturating_sub(overhead).max(1);
    if block.body.len() <= max_body {
        return vec![block.clone()];
    }
    split_body_bytes(&block.body, max_body)
        .into_iter()
        .map(|body| HistoryBlock {
            heading: block.heading.clone(),
            body,
        })
        .collect()
}

fn split_body_bytes(text: &str, max_bytes: usize) -> Vec<String> {
    let max_bytes = max_bytes.max(1);
    let mut parts = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + 1;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
        }
        if end < text.len() {
            let search_start = start + max_bytes / 2;
            if let Some(offset) = text[start..end].rfind('\n') {
                let candidate = start + offset + 1;
                if candidate > search_start {
                    end = candidate;
                }
            }
        }
        parts.push(text[start..end].to_string());
        start = end;
    }
    parts
}

fn pack_turns(turns: Vec<Vec<HistoryBlock>>, limit: usize) -> Vec<Vec<HistoryBlock>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut cost = 0;
    for turn in turns {
        let turn_cost = turn.iter().map(HistoryBlock::cost).sum::<usize>();
        if !current.is_empty() && cost + turn_cost > limit {
            groups.push(std::mem::take(&mut current));
            cost = 0;
        }
        current.extend(turn);
        cost += turn_cost;
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
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
            notification_pending: false,
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

    fn test_session() -> SessionRecord {
        SessionRecord {
            instance_id: "instance".to_string(),
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
        }
    }

    fn test_message(role: &str, text: &str) -> MessageWithParts {
        MessageWithParts {
            info: crate::opencode::types::MessageInfo {
                id: format!("msg_{role}"),
                role: role.to_string(),
                time: Default::default(),
                tokens: Default::default(),
            },
            parts: vec![crate::opencode::types::Part {
                kind: "text".to_string(),
                text: Some(text.to_string()),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn history_includes_session_info_roles_and_text() {
        let session = test_session();
        let messages = vec![
            test_message("user", "hello"),
            test_message("assistant", "hi there"),
        ];

        let pastes = history_pastes(&session, &messages, 4096);
        assert_eq!(pastes.len(), 1);
        let history = &pastes[0];
        assert!(history.contains("# Test"));
        assert!(history.contains("Session: ses_1"));
        assert!(history.contains("Directory: /tmp"));
        assert!(history.contains("Part: 1/1"));
        assert!(history.contains("## User\n\nhello"));
        assert!(history.contains("## Assistant\n\nhi there"));
    }

    #[test]
    fn history_splits_at_user_messages() {
        let session = test_session();
        let long = "a".repeat(300);
        let messages = vec![
            test_message("user", &long),
            test_message("assistant", &long),
            test_message("user", &long),
            test_message("assistant", &long),
        ];

        let pastes = history_pastes(&session, &messages, 700);
        assert!(pastes.len() > 1);
        for paste in &pastes {
            assert!(paste.len() <= 700);
            assert!(paste.contains("Session: ses_1"));
        }
        assert!(pastes[0].contains("## User\n\n"));
        assert!(pastes[0].contains(&long));
        assert!(!pastes[0].contains(&"## User\n\n".repeat(2)));
        assert!(pastes[1].contains("## User\n\n"));
    }

    #[test]
    fn history_splits_oversized_messages() {
        let session = test_session();
        let messages = vec![test_message("assistant", &"line\n".repeat(200))];

        let pastes = history_pastes(&session, &messages, 600);
        assert!(pastes.len() > 1);
        for paste in &pastes {
            assert!(paste.len() <= 600);
            assert!(paste.contains("## Assistant"));
            assert!(paste.contains("Session: ses_1"));
        }
    }

    fn parts_message(role: &str, parts: Vec<Part>) -> MessageWithParts {
        MessageWithParts {
            info: crate::opencode::types::MessageInfo {
                id: format!("msg_{role}"),
                role: role.to_string(),
                time: Default::default(),
                tokens: Default::default(),
            },
            parts,
        }
    }

    fn render_history(messages: &[MessageWithParts]) -> String {
        history_pastes(&test_session(), messages, 1_000_000).join("\n")
    }

    #[test]
    fn history_includes_reasoning() {
        let history = render_history(&[parts_message(
            "assistant",
            vec![
                Part {
                    kind: "reasoning".to_string(),
                    text: Some("weighing options".to_string()),
                    ..Default::default()
                },
                Part {
                    kind: "text".to_string(),
                    text: Some("done".to_string()),
                    ..Default::default()
                },
            ],
        )]);

        assert!(history.contains("### Reasoning\n\nweighing options"));
        assert!(history.contains("## Assistant\n\ndone"));
    }

    #[test]
    fn history_includes_completed_and_failed_tools() {
        let history = render_history(&[parts_message(
            "assistant",
            vec![
                Part {
                    kind: "tool".to_string(),
                    tool: Some("bash".to_string()),
                    call_id: Some("call_1".to_string()),
                    state: Some(
                        serde_json::from_value(serde_json::json!({
                            "status": "completed",
                            "input": {"command": "ls"},
                            "output": "file.txt"
                        }))
                        .unwrap(),
                    ),
                    ..Default::default()
                },
                Part {
                    kind: "tool".to_string(),
                    tool: Some("edit".to_string()),
                    state: Some(
                        serde_json::from_value(serde_json::json!({
                            "status": "error",
                            "input": {"path": "a.rs"},
                            "error": "permission denied"
                        }))
                        .unwrap(),
                    ),
                    ..Default::default()
                },
            ],
        )]);

        assert!(history.contains("### Tool: bash"));
        assert!(history.contains("Status: completed"));
        assert!(history.contains("Call: call_1"));
        assert!(history.contains("\"command\": \"ls\""));
        assert!(history.contains("Output:\nfile.txt"));
        assert!(history.contains("### Tool: edit"));
        assert!(history.contains("Status: error"));
        assert!(history.contains("Error:\npermission denied"));
    }

    #[test]
    fn history_includes_files_agents_and_subtasks() {
        let history = render_history(&[parts_message(
            "user",
            vec![
                Part {
                    kind: "file".to_string(),
                    filename: Some("diagram.png".to_string()),
                    mime: Some("image/png".to_string()),
                    url: Some("data:image/png;base64,AAAA".to_string()),
                    ..Default::default()
                },
                Part {
                    kind: "agent".to_string(),
                    name: Some("reviewer".to_string()),
                    ..Default::default()
                },
                Part {
                    kind: "subtask".to_string(),
                    agent: Some("explore".to_string()),
                    description: Some("look around".to_string()),
                    prompt: Some("find the entrypoint".to_string()),
                    ..Default::default()
                },
            ],
        )]);

        assert!(history.contains("### File: diagram.png"));
        assert!(history.contains("Type: image/png"));
        assert!(history.contains("URL: data:image/png;base64,… (4 bytes)"));
        assert!(history.contains("### Agent: reviewer"));
        assert!(history.contains("### Subtask: explore"));
        assert!(history.contains("Description: look around"));
        assert!(history.contains("Prompt:\nfind the entrypoint"));
    }

    #[test]
    fn history_includes_metadata_parts_and_omits_internal_ones() {
        let history = render_history(&[parts_message(
            "assistant",
            vec![
                Part {
                    kind: "step-start".to_string(),
                    ..Default::default()
                },
                Part {
                    kind: "step-finish".to_string(),
                    reason: Some("stop".to_string()),
                    cost: Some(0.1),
                    tokens: Some(crate::opencode::types::MessageTokens {
                        input: 10,
                        output: 5,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Part {
                    kind: "patch".to_string(),
                    hash: Some("abc123".to_string()),
                    files: vec!["a.rs".to_string()],
                    ..Default::default()
                },
                Part {
                    kind: "retry".to_string(),
                    attempt: Some(2),
                    error: Some(serde_json::json!({"data": {"message": "rate limited"}})),
                    ..Default::default()
                },
                Part {
                    kind: "compaction".to_string(),
                    auto: Some(true),
                    ..Default::default()
                },
                Part {
                    kind: "snapshot".to_string(),
                    ..Default::default()
                },
            ],
        )]);

        assert!(history.contains("### Step: stop"));
        assert!(history.contains("15 tokens · $0.1000"));
        assert!(history.contains("### Patch"));
        assert!(history.contains("- a.rs"));
        assert!(history.contains("Hash: abc123"));
        assert!(history.contains("### Retry #2"));
        assert!(history.contains("rate limited"));
        assert!(history.contains("### Compaction\n\nauto"));
        assert!(!history.contains("step-start"));
        assert!(!history.contains("### snapshot"));
    }

    #[test]
    fn history_falls_back_to_raw_json_for_unknown_parts() {
        let mut extra = serde_json::Map::new();
        extra.insert("foo".to_string(), serde_json::json!("bar"));
        let history = render_history(&[parts_message(
            "assistant",
            vec![Part {
                kind: "mystery".to_string(),
                extra,
                ..Default::default()
            }],
        )]);

        assert!(history.contains("### mystery"));
        assert!(history.contains("\"foo\": \"bar\""));
    }

    #[test]
    fn history_skips_ignored_text_parts() {
        let messages = vec![parts_message(
            "assistant",
            vec![Part {
                kind: "text".to_string(),
                text: Some("hidden".to_string()),
                ignored: true,
                ..Default::default()
            }],
        )];

        assert!(history_pastes(&test_session(), &messages, 4096).is_empty());
    }
}
