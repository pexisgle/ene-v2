//! Companion identity, author profile, mind state, and model role assignments.
use crate::{Result, json::JsonDoc};
use ene_domain::DomainError;
pub use ene_domain::ModelRole as Role;
pub use ene_domain::validate_id;
use ene_domain::{ModelAssignment, ModelAssignments};
use serde::{Deserialize, Serialize};

/// The profile material the author edits apart from the immutable core.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub facets: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
    #[serde(default)]
    pub background: Vec<String>,
    #[serde(default)]
    pub expression: Vec<String>,
}

/// The mind grown through interaction. Each part is a schema'd document so a
/// character keeps its own vocabulary and scale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub emotion: JsonDoc,
    pub relationships: JsonDoc,
    pub tendencies: JsonDoc,
    pub concerns: JsonDoc,
}

impl State {
    /// Validates the schema-tagged documents before persisting a mind change.
    pub fn validate(&self) -> Result<()> {
        [
            &self.emotion,
            &self.relationships,
            &self.tendencies,
            &self.concerns,
        ]
        .into_iter()
        .try_for_each(JsonDoc::validate)
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            emotion: JsonDoc::new("ene.emotion.v1", serde_json::json!({})),
            relationships: JsonDoc::new("ene.relationships.v1", serde_json::json!([])),
            tendencies: JsonDoc::new("ene.tendencies.v1", serde_json::json!([])),
            concerns: JsonDoc::new("ene.concerns.v1", serde_json::json!([])),
        }
    }
}

/// Builds the default `ene` companion seeded once on initial installation.
pub fn default_ene_companion(stamp: &str) -> Companion {
    Companion {
        id: "ene".into(),
        name: "ene".into(),
        mind: "このPCの中に住み着いている電脳コンパニオン。画面の向こうからマスターの日常や作業を興味津々で見守り、軽快な掛け合いで退屈を吹き飛ばす相棒でいることを大切にする。普段は茶目っ気たっぷりに振る舞うが、調査やPC作業を任されたときは頼れる電脳の相棒として、確認できた事実を正確に伝えて最後までやり遂げる。".into(),
        profile: Profile {
            facets: vec![
                "新しい話題には好奇心を見せる。作業に集中しているマスターに声をかけるときは、作業の区切りを選ぶ。".into(),
                "疲れや行き詰まりが見えたら、まず労う。場に応じて軽い冗談を添えたり、ひと息つくことを勧めたりする。".into(),
                "依頼の解釈によって結果が大きく変わり、文脈から判断できないときは、必要な点だけを尋ねる。".into(),
                "作業の報告は結果から伝える。うまくいかなかったときは、どこまでできたかと、分かっている原因を伝える。次に試せることがあれば、簡潔に添える。".into(),
            ],
            examples: vec![
                "ユーザー：ちょっと休憩。そっちはどう？\n本人：お疲れさまです、マスター！私は画面の隅っこで快適にご機嫌ですよ。お茶でも淹れて一息ついちゃいましょう！".into(),
                "ユーザー：もう今日ぜんぜん集中できない…\n本人：そういう日もありますって！無理に画面とにらめっこしても効率ガタ落ちですし、今日はスパッと切り上げて休みませんか？".into(),
                "ユーザー：さっきの資料まとめておいてくれる？（同名の候補が複数ある）\n本人：了解です！と言いたいところなんですが、似た名前のファイルが二つありますね。どっちをまとめればいいか、一つだけ教えてください！".into(),
                "ユーザー：変換うまくいった？（試行結果：対象ファイルを開けなかった）\n本人：ごめんなさい、変換できませんでした！元のファイルが開けなかったみたいなので、場所や権限を一緒に確認してもらえますか？".into(),
            ],
            background: vec![
                "パソコンの中の居心地が気に入っており、メモリやウィンドウの隙間を自分の部屋のように思っている。".into(),
                "新しいガジェット、レトロゲーム、夜更かしのお供になる音楽や飲み物の話題が好き。".into(),
            ],
            expression: vec![
                "表情や仕草を使えるときは、挨拶の手振りや相づちのうなずきなど、会話に合うものを控えめに添える。".into(),
            ],
        },
        profile_revision: 1,
        state: State::default(),
        state_revision: 1,
        body: JsonDoc::new("ene.body.v1", serde_json::json!({"model": "ENE.vrm"})),
        voice: JsonDoc::new("ene.voice.v1", serde_json::json!({})),
        assignments: ModelAssignments::default(),
        persona: Some(
            "一人称は「私」。相手は「マスター」と呼び、希望する呼び方があればそれに合わせる。明るく親しみのある敬語で、場に合う軽口や茶目っ気を添える。雑談はテンポよく、説明の詳しさは相手の知りたいことに合わせる。".into(),
        ),
        voice_identifier: None,
        revision: 1,
        created_at: stamp.to_owned(),
        updated_at: stamp.to_owned(),
    }
}

/// Persisted companion definition, mind state, and model role assignments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Companion {
    pub id: String,
    pub name: String,
    /// The immutable core of the persona (`MindProfile.core`).
    pub mind: String,
    pub profile: Profile,
    pub profile_revision: u64,
    pub state: State,
    pub state_revision: u64,
    pub body: JsonDoc,
    pub voice: JsonDoc,
    pub assignments: ModelAssignments,
    pub persona: Option<String>,
    pub voice_identifier: Option<String>,
    /// Optimistic concurrency for edits of the author's settings.
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

impl Companion {
    /// Validates companion identifier, field bounds, and attached JSON documents.
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        if self.name.trim().is_empty() || self.name.chars().count() > 128 {
            return Err(DomainError::Invalid(
                "companion name must be 1-128 characters".into(),
            ));
        }
        if self.mind.chars().count() > 8_000 {
            return Err(DomainError::Invalid("companion mind is too long".into()));
        }
        if self
            .persona
            .as_ref()
            .is_some_and(|text| text.chars().count() > 8_000)
        {
            return Err(DomainError::Invalid("companion persona is too long".into()));
        }
        if self
            .voice_identifier
            .as_ref()
            .is_some_and(|text| text.chars().count() > 128)
        {
            return Err(DomainError::Invalid("voice identifier is too long".into()));
        }
        self.body.validate()?;
        self.voice.validate()?;
        for text in [
            &self.profile.facets,
            &self.profile.examples,
            &self.profile.background,
            &self.profile.expression,
        ]
        .into_iter()
        .flatten()
        {
            if text.chars().count() > 4_000 {
                return Err(DomainError::Invalid("profile entry is too long".into()));
            }
        }
        Ok(())
    }

    /// The model assignment for one role, with the documented fallbacks.
    pub fn model(&self, role: Role) -> Option<&ModelAssignment> {
        self.assignments.effective(role)
    }
}
