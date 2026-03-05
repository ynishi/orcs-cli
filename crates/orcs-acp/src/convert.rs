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
