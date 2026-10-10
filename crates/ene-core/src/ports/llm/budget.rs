use super::{Generation, Result};
use crate::domain::Error;

/// Leave room for adapter fields and base64 inside the broker/IPC envelopes.
pub const MAX_GENERATION_BYTES: usize = 256 * 1024;

impl Generation {
    /// Drop oldest whole turns, preserving the preamble and current user input.
    /// Account for JSON escaping rather than estimating from raw text lengths.
    pub fn bound_history(&mut self) -> Result<()> {
        let mut bytes =
            ene_api_types::budget::encoded_len(self, usize::MAX).map_err(|_| Error::invalid())?;
        let current = self
            .messages
            .len()
            .checked_sub(1)
            .ok_or_else(Error::invalid)?;
        let mut removed = 0;
        while bytes > MAX_GENERATION_BYTES && removed < current {
            let next = (removed + 1..current)
                .find(|i| self.messages[*i].role == super::ChatRole::User)
                .unwrap_or(current);
            for message in &self.messages[removed..next] {
                bytes -= ene_api_types::budget::encoded_len(message, usize::MAX)
                    .map_err(|_| Error::invalid())?
                    + 1;
            }
            removed = next;
        }
        if bytes > MAX_GENERATION_BYTES {
            let mut error = Error::invalid();
            error.reason = Some("input_limit");
            return Err(error);
        }
        self.messages.drain(..removed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::llm::ChatMessage;
    fn request(messages: Vec<ChatMessage>) -> Generation {
        Generation {
            endpoint: "https://example.test/v1".into(),
            options: serde_json::json!({}),
            model: "model".into(),
            preamble: "keep instructions".into(),
            messages,
            max_tokens: 2048,
        }
    }
    #[test]
    fn escaped_history_is_bounded_in_whole_turns_without_changing_current_input() {
        let mut messages = vec![];
        for _ in 0..20 {
            messages.push(ChatMessage {
                role: crate::ports::llm::ChatRole::User,
                text: "\0".repeat(10_000),
            });
            messages.push(ChatMessage {
                role: crate::ports::llm::ChatRole::Assistant,
                text: "reply".into(),
            });
        }
        messages.push(ChatMessage {
            role: crate::ports::llm::ChatRole::User,
            text: "current".into(),
        });
        let mut r = request(messages);
        r.bound_history().unwrap();
        assert!(serde_json::to_vec(&r).unwrap().len() <= MAX_GENERATION_BYTES);
        assert_eq!(r.preamble, "keep instructions");
        assert_eq!(r.messages.last().unwrap().text, "current");
        assert_eq!(
            r.messages.first().unwrap().role,
            super::super::ChatRole::User
        );
        assert_eq!(r.messages.len() % 2, 1);
        assert!(r.messages.len() > 1 && r.messages.len() < 41);
    }
    #[test]
    fn current_input_is_rejected_instead_of_silently_truncated() {
        let mut r = request(vec![ChatMessage {
            role: crate::ports::llm::ChatRole::User,
            text: "\0".repeat(65_000),
        }]);
        assert_eq!(r.bound_history().unwrap_err().reason, Some("input_limit"));
        assert_eq!(r.messages[0].text.len(), 65_000);
    }
}
