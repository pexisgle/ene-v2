//! Platform boundaries shared by the host, desktop and plugin supervisor.
pub mod paths;
pub mod storage;

/// Portable package paths use forward slashes, never native device paths.
pub fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(':')
                && !part.contains(['<', '>', '"', '|', '?', '*'])
                && !part.ends_with(['.', ' '])
                && !matches!(
                    part.split('.')
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches(' ')
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                        | "CONIN$"
                        | "CONOUT$"
                        | "COM¹"
                        | "COM²"
                        | "COM³"
                        | "LPT¹"
                        | "LPT²"
                        | "LPT³"
                )
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_paths_cannot_name_devices_or_streams() {
        for path in [
            "../x", "/x", "C:/x", "x:stream", "x\\y", "a/./b", "a//b", "NUL.txt", "com1", "a.",
            "a ",
        ] {
            assert!(!super::relative(path), "{path}");
        }
        assert!(super::relative("日本語/main.wasm"));
    }
}
