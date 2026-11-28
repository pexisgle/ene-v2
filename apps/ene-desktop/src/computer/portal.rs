use super::{Result, encode};
use anyhow::{Context, ensure};
use ashpd::desktop::screenshot::Screenshot;
use ene_rpc::ScreenFrame;
use image::{ImageReader, Limits};
use rustix::fs::{Mode, OFlags, open};
use std::{
    io::{Cursor, Read as _},
    time::Duration,
};

pub(super) async fn capture() -> Result<ScreenFrame> {
    let request = Screenshot::request().interactive(true).modal(true);
    let reply = tokio::time::timeout(Duration::from_secs(45), request.send()).await??;
    let uri = reply.response()?.uri().as_str().to_owned();
    tokio::task::spawn_blocking(move || decode(&uri)).await?
}

fn decode(uri: &str) -> Result<ScreenFrame> {
    const MAX_FILE: u64 = 16 * 1024 * 1024;
    let path = url::Url::parse(uri)?
        .to_file_path()
        .ok()
        .context("portal did not return a local image")?;
    let file: std::fs::File = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?
    .into();
    ensure!(
        file.metadata()?.is_file(),
        "portal response is not a regular file"
    );
    let mut data = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_FILE,
        "portal image exceeds size limit"
    );
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(192 * 1024 * 1024);
    reader.limits(limits);
    encode(reader.decode()?.into_rgb8())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a private desktop bus with a screenshot portal"]
    async fn requests_and_encodes_an_interactive_portal_capture() -> Result<()> {
        let frame = capture().await?;
        assert!(frame.width > 0 && frame.height > 0);
        assert!(frame.jpeg.starts_with(&[0xff, 0xd8]));
        Ok(())
    }

    #[test]
    fn rejects_nonlocal_portal_files() {
        assert!(decode("https://example.com/private.png").is_err());
        assert!(decode("file://elsewhere/private.png").is_err());
    }
}
