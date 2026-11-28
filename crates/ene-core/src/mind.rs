use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mind {
    pub emotion: String,
    pub relationship: String,
    pub tendencies: Vec<String>,
    pub interests: Vec<String>,
    pub observed_at: i64,
}

impl Mind {
    pub fn at(&self, now: i64) -> Self {
        let mut state = self.clone();
        if now.saturating_sub(self.observed_at) >= 6 * 3600 {
            state.emotion.clear();
        }
        state
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Trigger,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    pub id: String,
    pub turn_id: String,
    pub role: Role,
    pub text: String,
    pub time: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Summary {
    pub through: u64,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Record {
    Entry {
        message: Message,
        receipt: Option<Receipt>,
    },
    Summary(Summary),
    Injection {
        turn_id: String,
        memories: Vec<(String, i64)>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub key: String,
    pub fingerprint: String,
}

pub struct Prompt<'a> {
    pub system: &'a str,
    pub summary: &'a str,
    pub history: &'a [Message],
    pub dynamic: &'a str,
    pub input: &'a str,
}

impl<'a> Prompt<'a> {
    pub fn fit(mut self, budget: usize) -> Result<Self> {
        // UTF-8 bytes give a conservative bound for byte-based tokenizers.
        let fixed =
            self.system.len() + self.summary.len() + self.dynamic.len() + self.input.len() + 64;
        ensure!(fixed <= budget, "input exceeds model context");
        let mut total = fixed;
        let mut offset = self.history.len();
        for index in (0..self.history.len()).rev() {
            let message = &self.history[index];
            total += message.text.len() + 16;
            if total > budget {
                break;
            }
            if message.role != Role::Assistant {
                offset = index;
            }
        }
        self.history = &self.history[offset..];
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub text: String,
    pub importance: u8,
    pub shared: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reflection {
    pub summary: String,
    pub facts: Vec<Fact>,
    pub mind: Mind,
    #[serde(default)]
    pub cited_memories: Vec<String>,
}

pub fn recall_rank<'a, K>(ranked: [impl IntoIterator<Item = &'a K>; 2]) -> HashMap<&'a K, f64>
where
    K: Eq + std::hash::Hash + 'a,
{
    let mut scores = HashMap::new();
    for index in ranked {
        for (offset, key) in index.into_iter().enumerate() {
            *scores.entry(key).or_default() += 1.0 / (60.0 + offset as f64);
        }
    }
    scores
}

pub fn recall_score(relevance: f64, importance: u8, last_used: i64, now: i64) -> f64 {
    let hours = now.saturating_sub(last_used).max(0) as f64 / 3600.0;
    let weight = f64::from(importance.min(10));
    relevance * weight * (-hours / (24.0 * (1.0 + weight))).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_turn_boundaries() -> Result<()> {
        let history = [
            Message {
                id: "1".into(),
                turn_id: String::new(),
                role: Role::User,
                text: "a".repeat(200),
                time: 0,
            },
            Message {
                id: "2".into(),
                turn_id: String::new(),
                role: Role::Assistant,
                text: "ok".into(),
                time: 0,
            },
            Message {
                id: "3".into(),
                turn_id: String::new(),
                role: Role::User,
                text: "next".into(),
                time: 0,
            },
            Message {
                id: "4".into(),
                turn_id: String::new(),
                role: Role::Assistant,
                text: "ok".into(),
                time: 0,
            },
        ];
        let prompt = Prompt {
            system: "profile",
            summary: "",
            history: &history,
            dynamic: "now",
            input: "hello",
        };
        assert_eq!(prompt.fit(160)?.history[0].id, "3");
        Ok(())
    }
}
