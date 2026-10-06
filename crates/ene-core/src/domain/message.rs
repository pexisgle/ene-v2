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
        if matches!(message.status, MessageStatus::Unknown(_)) {
            return Err(Error::new(ErrorKind::UnsupportedCapability));
        }
        if message.role != Role::Companion {
            return Err(Error::new(ErrorKind::StateConflict));
        }
        let status = match (&message.status, self) {
            (MessageStatus::Queued, Self::Start) => MessageStatus::Streaming,
            (MessageStatus::Streaming, Self::Append { offset, text }) => {
                if *offset != message.text.chars().count()
                    || message.text.len().saturating_add(text.len())
                        > OperationKind::Conversation.spec().output_bytes
                {
                    return Err(Error::new(ErrorKind::StateConflict));
                }
                message.text.push_str(text);
                MessageStatus::Streaming
            }
            (MessageStatus::Streaming, Self::Complete) => MessageStatus::Completed,
            (MessageStatus::Queued | MessageStatus::Streaming, Self::Fail(_)) => {
                MessageStatus::Failed
            }
            (MessageStatus::Queued | MessageStatus::Streaming, Self::Cancel(_)) => {
                MessageStatus::Cancelled
            }
            _ => return Err(Error::new(ErrorKind::StateConflict)),
        };
        message.failure = match self {
            Self::Fail(failure) | Self::Cancel(failure) => Some(failure.clone()),
            _ => None,
        };
        message.status = status;
        Ok(())
    }

    /// Confirmation of a possibly committed terminal write is separate from transition.
    pub fn is_confirmed(&self, message: &Message) -> bool {
        if message.role != Role::Companion {
            return false;
        }
        match self {
            Self::Complete => {
                message.status == MessageStatus::Completed && message.failure.is_none()
            }
            Self::Fail(failure) => {
                message.status == MessageStatus::Failed && message.failure.as_ref() == Some(failure)
            }
            Self::Cancel(failure) => {
                message.status == MessageStatus::Cancelled
                    && message.failure.as_ref() == Some(failure)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(role: Role, status: MessageStatus) -> Message {
        serde_json::from_value(serde_json::json!({"id":"m","companion_id":"c","role":role,"status":status,"text":"","created_at":"now","sequence":1,"resources":[],"in_reply_to":null,"task_id":null,"question_id":null,"failure":null})).unwrap()
    }
    fn failure() -> Failure {
        Failure {
            code: "failed".into(),
            message: "reason".into(),
            details: serde_json::json!({}),
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
        for role in [Role::User, Role::Companion, Role::Unknown("future".into())] {
            for status in [
                MessageStatus::Queued,
                MessageStatus::Streaming,
                MessageStatus::Completed,
                MessageStatus::Failed,
                MessageStatus::Cancelled,
                MessageStatus::Unknown("future".into()),
            ] {
                for (index, update) in updates.iter().enumerate() {
                    let mut message = row(role.clone(), status.clone());
                    let original = message.clone();
                    let expected = role == Role::Companion
                        && match status {
                            MessageStatus::Queued => matches!(index, 0 | 3 | 4),
                            MessageStatus::Streaming => index != 0,
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
        let mut message = row(Role::Companion, MessageStatus::Queued);
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
        message.failure.as_mut().unwrap().message = "different".into();
        assert!(!terminal.is_confirmed(&message));
        assert!(!MessageUpdate::Complete.is_confirmed(&row(Role::User, MessageStatus::Completed)));
    }
}
