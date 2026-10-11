use super::*;

pub(super) fn show(ui: &AppWindow) {
    if let Err(error) = ui.show() {
        eprintln!("画面を表示できません: {error}");
    }
    ui.window().set_minimized(false);
    ui.window().request_redraw();
}
pub(super) fn focus_page(ui: &AppWindow) {
    ui.invoke_focus_entry();
}
pub fn show_host_selection(ui: &AppWindow) {
    if ui.global::<UiState>().get_quit_confirmation() {
        show(ui);
        return;
    }
    ui.global::<UiState>().set_category(4);
    navigate(ui, 1);
}
pub fn navigate(ui: &AppWindow, page: i32) {
    if ui.global::<UiState>().get_quit_confirmation() {
        show(ui);
        return;
    }
    let page = page.clamp(0, 1);
    if page == 0 && !ui.global::<UiState>().get_session_active() {
        ui.global::<UiState>().set_host_selection_visible(true);
        show(ui);
        focus_page(ui);
        return;
    }
    ui.global::<UiState>().set_host_selection_visible(false);
    ui.global::<UiState>().set_page(page.clamp(0, 1));
    show(ui);
    focus_page(ui);
}
