pub(super) fn parse_wm_class(bytes: &[u8]) -> String {
    let mut parts = bytes
        .split(|&byte| byte == 0)
        .map(|part| String::from_utf8_lossy(part).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    if !class.is_empty() { class } else { instance }
}

pub(super) fn wayland_state_is_activated(bytes: &[u8]) -> bool {
    const ACTIVATED: u32 = 2;
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .any(|chunk| u32::from_ne_bytes(*chunk) == ACTIVATED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_wm_class_prefers_class_and_falls_back_to_instance() {
        assert_eq!(parse_wm_class(b"kitty\0kitty\0"), "kitty");
        assert_eq!(parse_wm_class(b"navigator\0firefox\0"), "firefox");
        assert_eq!(parse_wm_class(b"only-instance\0\0"), "only-instance");
        assert_eq!(parse_wm_class(b""), "");
    }

    #[test]
    fn wayland_toplevel_state_detects_activated_entry() {
        let inactive = [0u32, 1u32]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        assert!(!wayland_state_is_activated(&inactive));

        let active = [0u32, 2u32, 3u32]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        assert!(wayland_state_is_activated(&active));

        // Truncated trailing bytes never trigger false activation or panic.
        assert!(!wayland_state_is_activated(&[2, 0, 0]));
    }
}
