use super::*;

pub(crate) struct VoiceTest {
    phrase: Entity<InputState>,
    pub(crate) recording: bool,
    pub(crate) level: u32,
    pub(crate) transcript: String,
}

impl VoiceTest {
    pub(super) fn new(window: &mut GpuiWindow, cx: &mut Context<Window>) -> Self {
        let phrase = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_value("こんにちは。音声をテストしています。", window, cx);
            input
        });
        Self {
            phrase,
            recording: false,
            level: 0,
            transcript: String::new(),
        }
    }

    pub(super) fn render(&self, cx: &Context<Window>) -> impl IntoElement {
        let phrase = self.phrase.clone();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child("音声認識")
            .child(
                Button::new("test-mic")
                    .label(if self.recording {
                        "録音を停止して認識"
                    } else {
                        "マイクをテスト"
                    })
                    .on_click(cx.listener(|owner, _, _, cx| owner.toggle_voice_test(cx))),
            )
            .when(self.recording, |view| {
                view.child(format!(
                    "入力レベル {}% · {}",
                    self.level,
                    if self.level >= 2 {
                        "発話を検出"
                    } else {
                        "待機中"
                    }
                ))
            })
            .when(!self.transcript.is_empty(), |view| {
                view.child(format!("認識結果 · {}", self.transcript))
            })
            .child("読み上げ")
            .child(Input::new(&self.phrase))
            .child(
                Button::new("test-voice")
                    .label("テスト文を読み上げ")
                    .on_click(cx.listener(move |owner, _, _, cx| {
                        owner.test_speech(value(&phrase, cx), cx);
                    })),
            )
    }
}
