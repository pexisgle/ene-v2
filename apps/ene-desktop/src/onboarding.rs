use crate::{
    network::{Action, id},
    window::{MILK, PETAL, Window, value},
};
use ene_rpc::{self as w, model_selection::Role};
use gpui::base::Disableable as _;
use gpui::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
};
use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Styled as _,
    Window as GpuiWindow, div, prelude::FluentBuilder as _, px, rgb,
};

pub(crate) struct Onboarding {
    endpoint: Entity<InputState>,
    secret: Entity<InputState>,
    model: Entity<InputState>,
    connection_id: String,
    busy: bool,
}

impl Onboarding {
    pub(crate) fn new(window: &mut GpuiWindow, cx: &mut Context<Window>) -> Self {
        Self {
            endpoint: cx
                .new(|cx| InputState::new(window, cx).placeholder("https://api.example.org/v1")),
            secret: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("API キー（必要な場合）")
                    .masked(true)
            }),
            model: cx.new(|cx| InputState::new(window, cx).placeholder("モデル ID")),
            connection_id: id(),
            busy: false,
        }
    }

    pub(crate) fn idle(&mut self) {
        self.busy = false;
    }

    pub(crate) fn render(
        &self,
        settings: &w::Settings,
        models: &[String],
        companion: &w::Companion,
        cx: &Context<Window>,
    ) -> impl IntoElement {
        let selected = settings
            .connections
            .iter()
            .filter(|connection| {
                connection.enabled
                    && matches!(
                        connection.kind.as_known(),
                        Some(w::ProviderKind::OpenaiCompatible | w::ProviderKind::Plugin)
                    )
            })
            .find(|connection| connection.id == self.connection_id)
            .or_else(|| {
                settings.connections.iter().find(|connection| {
                    connection.enabled
                        && matches!(
                            connection.kind.as_known(),
                            Some(w::ProviderKind::OpenaiCompatible | w::ProviderKind::Plugin)
                        )
                })
            })
            .cloned();
        let endpoint = self.endpoint.clone();
        let secret = self.secret.clone();
        let busy = self.busy;
        let connection_id = self.connection_id.clone();
        let form = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("1. プロバイダーを接続"),
            )
            .child(
                div()
                    .text_sm()
                    .child("対話・作業・安全判定の文脈が、登録した接続先に送信されます。"),
            )
            .when(selected.is_none(), |form| form.child(Input::new(&endpoint)))
            .when_some(selected.as_ref(), |form, connection| {
                form.child(div().text_sm().child(connection.endpoint.clone()))
            })
            .child(Input::new(&secret))
            .child(
                Button::new("setup-connection")
                    .label(if busy {
                        "処理中…"
                    } else if selected.is_some() {
                        "認証情報を保存してモデルを取得"
                    } else {
                        "接続を保存"
                    })
                    .disabled(busy)
                    .on_click(cx.listener({
                        let selected = selected.clone();
                        move |this, _, window, cx| {
                            let connection = selected.clone().unwrap_or_else(|| w::Connection {
                                id: connection_id.clone(),
                                enabled: true,
                                kind: w::ProviderKind::OpenaiCompatible.into(),
                                name: "OpenAI 互換".into(),
                                endpoint: value(&endpoint, cx),
                                send_data: vec!["対話・作業・安全判定の文脈".into()],
                                ..Default::default()
                            });
                            if connection.endpoint.is_empty() {
                                return;
                            }
                            let credential = secret.read(cx).unmask_value().to_string();
                            secret.update(cx, |input, cx| input.set_value("", window, cx));
                            this.start_setup(
                                Action::SetupProvider(
                                    connection,
                                    (!credential.is_empty()).then_some(credential),
                                ),
                                cx,
                            );
                        }
                    })),
            );
        let items = models.iter().take(8).cloned().map(|model| {
            let input = self.model.clone();
            Button::new(format!("setup-choice-{model}"))
                .label(model.clone())
                .on_click(cx.listener(move |_, _, window, cx| {
                    input.update(cx, |input, cx| input.set_value(model.clone(), window, cx));
                }))
        });
        let model = self.model.clone();
        let companion = companion.clone();
        div()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .min_h_0()
            .flex_1()
            .child(
                div()
                    .p_4()
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(PETAL))
                    .bg(rgb(MILK))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("初回セットアップ"),
                    )
                    .child(form)
                    .when_some(selected, |view, selected| {
                        view.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .border_t_1()
                                .border_color(rgb(PETAL))
                                .pt_3()
                                .child(
                                    div()
                                        .text_base()
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .child("2. 最初に使うモデルを選択"),
                                )
                                .child(div().flex().gap_2().flex_wrap().children(items))
                                .child(Input::new(&model))
                                .child(
                                    Button::new("setup-apply")
                                        .label("接続を確認して対話を始める")
                                        .primary()
                                        .disabled(busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let model = value(&model, cx);
                                            if model.is_empty() {
                                                return;
                                            }
                                            this.start_setup(
                                                Action::SetupModel(
                                                    companion.clone(),
                                                    w::ModelSelection {
                                                        role: Role::Dialogue.into(),
                                                        connection_id: selected.id.clone(),
                                                        model,
                                                        ..Default::default()
                                                    },
                                                ),
                                                cx,
                                            );
                                        })),
                                ),
                        )
                    }),
            )
            .max_w(px(640.0))
    }

    pub(crate) fn started(&mut self) {
        self.busy = true;
    }
}
