//! Conversion between ACP types and ORCS types.

use agent_client_protocol::{ContentBlock, ContentChunk, SessionUpdate};

/// Extracts plain text from ACP prompt content blocks.
///
/// Concatenates all `Text` content blocks with newlines.
/// Non-text blocks (images, audio, resources) are ignored in P1.
pub fn extract_text_from_prompt(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.as_str());
        }
    }
    parts.join("\n")
}

/// Converts ORCS output text into an ACP `SessionUpdate::AgentMessageChunk`.
pub fn text_to_agent_message_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(text.into()))
}

/// Converts ORCS output text into an ACP `SessionUpdate::AgentThoughtChunk`.
pub fn text_to_agent_thought_chunk(text: &str) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(text.into()))
}

/// Converts an `IOOutput` into zero or more ACP `SessionUpdate`s.
///
/// Returns `None` for output types that have no ACP representation
/// (e.g., `Clear`, `Prompt`).
pub fn io_output_to_session_update(output: &orcs_runtime::IOOutput) -> Option<SessionUpdate> {
    match output {
        orcs_runtime::IOOutput::Print { text, style } => {
            let update = match style {
                orcs_runtime::OutputStyle::Debug => text_to_agent_thought_chunk(text),
                _ => text_to_agent_message_chunk(text),
            };
            Some(update)
        }
        orcs_runtime::IOOutput::ShowProcessing {
            component,
            operation,
        } => Some(text_to_agent_thought_chunk(&format!(
            "[{component}] Processing ({operation})..."
        ))),
        orcs_runtime::IOOutput::ShowApprovalRequest {
            id,
            operation,
            description,
        } => Some(text_to_agent_message_chunk(&format!(
            "[{operation}] {id} - {description}"
        ))),
        orcs_runtime::IOOutput::ShowApproved { approval_id } => Some(text_to_agent_message_chunk(
            &format!("Approved: {approval_id}"),
        )),
        orcs_runtime::IOOutput::ShowRejected {
            approval_id,
            reason,
        } => {
            let msg = match reason {
                Some(r) => format!("Rejected: {approval_id} ({r})"),
                None => format!("Rejected: {approval_id}"),
            };
            Some(text_to_agent_message_chunk(&msg))
        }
        orcs_runtime::IOOutput::Prompt { .. } | orcs_runtime::IOOutput::Clear => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::TextContent;

    #[test]
    fn extract_text_single_block() {
        let blocks = vec![ContentBlock::Text(TextContent::new("hello"))];
        assert_eq!(extract_text_from_prompt(&blocks), "hello");
    }

    #[test]
    fn extract_text_multiple_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ];
        assert_eq!(extract_text_from_prompt(&blocks), "first\nsecond");
    }

    #[test]
    fn extract_text_empty() {
        let blocks: Vec<ContentBlock> = vec![];
        assert_eq!(extract_text_from_prompt(&blocks), "");
    }

    #[test]
    fn text_to_chunk_conversion() {
        let update = text_to_agent_message_chunk("output text");
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                ContentBlock::Text(text) => {
                    assert_eq!(text.text, "output text");
                }
                other => panic!("expected Text content, got: {other:?}"),
            },
            other => panic!("expected AgentMessageChunk, got: {other:?}"),
        }
    }
}
