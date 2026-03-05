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

    #[test]
    fn text_to_thought_chunk_conversion() {
        let update = text_to_agent_thought_chunk("thinking");
        assert!(
            matches!(update, SessionUpdate::AgentThoughtChunk(_)),
            "expected AgentThoughtChunk, got: {update:?}"
        );
    }

    // --- io_output_to_session_update tests ---

    #[test]
    fn io_output_print_normal_is_message_chunk() {
        let output = orcs_runtime::IOOutput::print("hello");
        let update = io_output_to_session_update(&output)
            .expect("Print/Normal should produce a SessionUpdate");
        assert!(
            matches!(update, SessionUpdate::AgentMessageChunk(_)),
            "Normal print should be AgentMessageChunk"
        );
    }

    #[test]
    fn io_output_print_debug_is_thought_chunk() {
        let output = orcs_runtime::IOOutput::Print {
            text: "debug info".into(),
            style: orcs_runtime::OutputStyle::Debug,
        };
        let update = io_output_to_session_update(&output)
            .expect("Print/Debug should produce a SessionUpdate");
        assert!(
            matches!(update, SessionUpdate::AgentThoughtChunk(_)),
            "Debug print should be AgentThoughtChunk"
        );
    }

    #[test]
    fn io_output_show_processing_is_thought_chunk() {
        let output = orcs_runtime::IOOutput::ShowProcessing {
            component: "shell".into(),
            operation: "executing".into(),
        };
        let update = io_output_to_session_update(&output)
            .expect("ShowProcessing should produce a SessionUpdate");
        assert!(
            matches!(update, SessionUpdate::AgentThoughtChunk(_)),
            "ShowProcessing should be AgentThoughtChunk"
        );
    }

    #[test]
    fn io_output_approval_request_is_message_chunk() {
        let output = orcs_runtime::IOOutput::ShowApprovalRequest {
            id: "req-1".into(),
            operation: "write".into(),
            description: "write file".into(),
        };
        let update = io_output_to_session_update(&output)
            .expect("ShowApprovalRequest should produce a SessionUpdate");
        assert!(
            matches!(update, SessionUpdate::AgentMessageChunk(_)),
            "ShowApprovalRequest should be AgentMessageChunk"
        );
    }

    #[test]
    fn io_output_approved_is_message_chunk() {
        let output = orcs_runtime::IOOutput::ShowApproved {
            approval_id: "req-1".into(),
        };
        let update = io_output_to_session_update(&output)
            .expect("ShowApproved should produce a SessionUpdate");
        assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn io_output_rejected_with_reason() {
        let output = orcs_runtime::IOOutput::ShowRejected {
            approval_id: "req-2".into(),
            reason: Some("not allowed".into()),
        };
        let update = io_output_to_session_update(&output)
            .expect("ShowRejected should produce a SessionUpdate");
        assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn io_output_rejected_without_reason() {
        let output = orcs_runtime::IOOutput::ShowRejected {
            approval_id: "req-3".into(),
            reason: None,
        };
        let update = io_output_to_session_update(&output)
            .expect("ShowRejected without reason should produce a SessionUpdate");
        assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn io_output_prompt_returns_none() {
        let output = orcs_runtime::IOOutput::prompt("orcs>");
        assert!(
            io_output_to_session_update(&output).is_none(),
            "Prompt should have no ACP representation"
        );
    }

    #[test]
    fn io_output_clear_returns_none() {
        let output = orcs_runtime::IOOutput::Clear;
        assert!(
            io_output_to_session_update(&output).is_none(),
            "Clear should have no ACP representation"
        );
    }
}
