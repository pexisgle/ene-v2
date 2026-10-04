use super::*;

pub(crate) fn bind_chat(ui: &ChatWindow, mode: Mode) -> Rc<VecModel<ChatEntry>> {
    ui.set_demo_mode(mode == Mode::Demo);
    ui.set_status(if mode == Mode::Demo {
        "日本語入力を試せます。返答は仮応答です。".into()
    } else {
        "未接続 — ホスト接続は第3段階で実装します。".into()
    });
    let messages = Rc::new(VecModel::<ChatEntry>::default());
    ui.set_messages(messages.clone().into());
    let weak = ui.as_weak();
    ui.on_draft_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            ui.set_can_send(mode.accepts(&text));
        }
    });
    let model = messages.clone();
    let weak = ui.as_weak();
    ui.on_submit(move || {
        let Some(ui) = weak.upgrade() else { return };
        let draft = ui.get_draft();
        let Some(response) = mode.respond(&draft) else {
            return;
        };
        model.push(ChatEntry {
            author: "あなた".into(),
            body: draft,
            provisional: false,
        });
        model.push(ChatEntry {
            author: "ene".into(),
            body: response.into(),
            provisional: true,
        });
        ui.set_draft("".into());
        ui.set_can_send(false);
        ui.set_status("仮応答を表示しました。".into());
        ui.invoke_focus_input();
        // Layout is deferred; scrolling now would use the old content height.
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_scroll_to_latest();
            }
        });
    });
    messages
}
