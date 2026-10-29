use super::*;

/// An update cannot combine a terminal status with text or overwrite a user row.
#[derive(Clone, Debug)]
pub enum MessageUpdate {
    Start,
    Append { offset: usize, text: String },
    Complete,
    Fail(Failure),
    Cancel(Failure),
}

impl MessageUpdate {
    pub fn apply(&self, message: &mut Message) -> Result<(), Error> {
        if !["queued", "streaming", "completed", "failed", "cancelled"]
            .contains(&message.status.as_str())
        {
            return Err(Error::new(ErrorKind::UnsupportedCapability));
        }
        if message.role != "companion" {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        let status = match (message.status.as_str(), self) {
            ("queued", Self::Start) => "streaming",
            ("streaming", Self::Append { offset, text }) => {
                if *offset != message.text.chars().count()
                    || message.text.len().saturating_add(text.len())
                        > OperationKind::Conversation.spec().output_bytes
                {
                    return Err(Error::new(ErrorKind::StateConflict));
                }
                message.text.push_str(text);
                "streaming"
            }
            ("streaming", Self::Complete) => "completed",
            ("queued" | "streaming", Self::Fail(_)) => "failed",
            ("queued" | "streaming", Self::Cancel(_)) => "cancelled",
            _ => return Err(Error::new(ErrorKind::StateConflict)),
        };
        message.failure = match self {
            Self::Fail(failure) | Self::Cancel(failure) => failure.clone().into(),
            _ => Default::default(),
        };
        message.status = status.into();
        Ok(())
    }

    /// Confirmation of a possibly committed terminal write is separate from transition.
    pub fn is_confirmed(&self, message: &Message) -> bool {
        if message.role != "companion" {
            return false;
        }
        match self {
            Self::Complete => message.status == "completed" && message.failure.is_unset(),
            Self::Fail(failure) => {
                message.status == "failed" && message.failure.as_option() == Some(failure)
            }
            Self::Cancel(failure) => {
                message.status == "cancelled" && message.failure.as_option() == Some(failure)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(role: &str, status: &str) -> Message {
        Message {
            id: "m".into(),
            companion_id: "c".into(),
            role: role.into(),
            status: status.into(),
            created_at: "now".into(),
            sequence: 1,
            ..Default::default()
        }
    }
    fn failure() -> Failure {
        Failure {
            code: "failed".into(),
            message: "reason".into(),
            details: ene_rpc::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }
    #[test]
    fn transition_matrix_never_updates_users_or_reopens_terminals() {
        let updates = [
            MessageUpdate::Start,
            MessageUpdate::Append {
                offset: 0,
                text: "日🙂e\u{301}".into(),
            },
            MessageUpdate::Complete,
            MessageUpdate::Fail(failure()),
            MessageUpdate::Cancel(failure()),
        ];
        for role in ["user", "companion", "future"] {
            for status in [
                "queued",
                "streaming",
                "completed",
                "failed",
                "cancelled",
                "future",
            ] {
                for (index, update) in updates.iter().enumerate() {
                    let mut message = row(role, status);
                    let original = message.clone();
                    let expected = role == "companion"
                        && match status {
                            "queued" => matches!(index, 0 | 3 | 4),
                            "streaming" => index != 0,
                            _ => false,
                        };
                    assert_eq!(
                        update.apply(&mut message).is_ok(),
                        expected,
                        "{role:?} {status:?} {update:?}"
                    );
                    if !expected {
                        assert_eq!(message, original);
                    }
                }
            }
        }
    }
    #[test]
    fn append_uses_scalar_offsets_and_terminal_confirmation_requires_exact_failure() {
        let mut message = row("companion", "queued");
        MessageUpdate::Start.apply(&mut message).unwrap();
        MessageUpdate::Append {
            offset: 0,
            text: "日🙂e\u{301}".into(),
        }
        .apply(&mut message)
        .unwrap();
        assert!(
            MessageUpdate::Append {
                offset: message.text.len(),
                text: "bad".into()
            }
            .apply(&mut message)
            .is_err()
        );
        MessageUpdate::Append {
            offset: 4,
            text: "。".into(),
        }
        .apply(&mut message)
        .unwrap();
        let terminal = MessageUpdate::Fail(failure());
        terminal.apply(&mut message).unwrap();
        assert!(terminal.apply(&mut message).is_err());
        assert!(terminal.is_confirmed(&message));
        message.failure.as_option_mut().unwrap().message = "different".into();
        assert!(!terminal.is_confirmed(&message));
        assert!(!MessageUpdate::Complete.is_confirmed(&row("user", "completed")));
    }
}
