//! Count the real JSON representation without allocating an encoded copy.
use serde::Serialize;
use std::io::{self, Write};
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HISTORY_PAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CONFIGURATION_BYTES: usize = 4 * 1024 * 1024;

pub fn encoded_len(value: &impl Serialize, limit: usize) -> Result<usize, serde_json::Error> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.limit {
                return Err(io::Error::other("resource_limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}
