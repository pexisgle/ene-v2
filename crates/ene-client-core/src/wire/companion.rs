//! Wire conversions for companion, mind, chat message, and turn types.

use crate::domain;
use ene_domain::CompanionId;
use ene_rpc as w;

from_fields! {
    w::MindProfile => domain::MindProfile {
        core, revision, facets, examples, background, expression,
    }
}

from_fields! {
    domain::MindProfile => w::MindProfile {
        core, revision, facets, examples, background, expression,
        ..Default::default()
    }
}

from_fields! {
    w::MindState => domain::MindState {
        revision,
        emotion.into_option().map(Into::into),
        relationships.into_option().map(Into::into),
        tendencies.into_option().map(Into::into),
        concerns.into_option().map(Into::into),
        updated_at,
    }
}

from_fields! {
    w::MindIndexStatus => domain::MindIndexStatus {
        pending_fts_count, pending_vector_count, embedding_model, model_calls,
        total_tokens,
    }
}

from_fields! {
    w::MindView => domain::MindView {
        companion_id,
        profile.into_option().map(Into::into),
        state.into_option().map(Into::into),
        index.into_option().map(Into::into),
    }
}

from_fields! {
    w::Companion => domain::Companion {
        id, name, mind,
        body.into_option().map(Into::into),
        voice.into_option().map(Into::into),
        assignments.into_option().map(Into::into),
        revision, created_at, updated_at, persona, voice_identifier,
    }
}

from_fields! {
    w::CompanionTemplate => domain::CompanionTemplate {
        format_version, id, name, mind, persona,
        profile.into_option().map(Into::into),
        body.into_option().map(Into::into),
        voice.into_option().map(Into::into),
        voice_identifier,
    }
}

from_fields! {
    domain::CompanionTemplate => w::CompanionTemplate {
        format_version, id, name, mind, persona,
        profile.map(Into::into).into(),
        body.map(Into::into).into(),
        voice.map(Into::into).into(),
        voice_identifier,
        ..Default::default()
    }
}

from_variants! {
    w::MessageRole => domain::MessageRole [
        Unspecified, User, Assistant, Agent, Context,
    ]
}

impl TryFrom<w::Message> for domain::Message {
    type Error = ene_domain::DomainError;

    fn try_from(value: w::Message) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            companion_id: Some(CompanionId::parse(value.companion_id)?),
            work_id: value.work_id,
            role: value.role.as_known().map_or_default(Into::into),
            text: value.text,
            created_at: value.created_at,
            sequence: value.sequence,
            request_id: value.request_id,
            author: value.author,
        })
    }
}

from_variants! {
    w::TurnStatus => domain::TurnStatus [
        Unspecified, Streaming, Answered, Stopped, Failed,
    ]
}

impl TryFrom<w::TurnEvent> for domain::TurnEvent {
    type Error = ene_domain::DomainError;

    fn try_from(value: w::TurnEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            companion_id: Some(CompanionId::parse(value.companion_id)?),
            turn_id: value.turn_id,
            status: value.status.as_known().map_or_default(Into::into),
            failure: value.failure.into_option().map(Into::into),
        })
    }
}
