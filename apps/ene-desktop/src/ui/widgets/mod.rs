//! Shared settings surfaces, buttons, pills and dropdowns.

mod buttons;
pub(super) use buttons::navigation_button;
mod dropdown;
mod notice;
mod pills;

pub use buttons::{
    ButtonStyle, CircleButtonStyle, Tone, circle_icon_button, floating_orb_button, pill_button,
    toggle_switch,
};
pub(crate) use dropdown::ChoiceValue;
pub use dropdown::{DropdownOption, DropdownSpec, dropdown_select};
pub(super) use notice::notice_strip;
pub use pills::{companion_badge, disclosure_row, kind_pill, status_pill, window_drag_area};

pub(super) fn surface_card(theme: &super::theme::Theme) -> gpui::Div {
    use gpui::Styled as _;

    gpui::div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(super::theme::radius::surface())
}
