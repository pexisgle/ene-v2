//! Small shared widgets: buttons, pills and the dropdown.
//!
//! Each submodule owns one family and re-exports its surface here, so

mod buttons;
pub(super) use buttons::navigation_button;
mod dropdown;
mod notice;
mod pills;

pub use buttons::{
    ButtonStyle, CircleButtonStyle, Tone, circle_icon_button, floating_orb_button, pill_button,
    toggle_switch,
};
pub use dropdown::{DropdownOption, DropdownSpec, dropdown_select};
pub(super) use notice::notice_strip;
pub use pills::{companion_badge, disclosure_row, kind_pill, status_pill, window_drag_area};
