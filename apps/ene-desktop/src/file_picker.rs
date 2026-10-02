//! OS file chooser; cancellation leaves the settings draft untouched.
use crate::SettingsWindow;
use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
use slint::ComponentHandle;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub fn bind(settings: &SettingsWindow) {
    let active = Arc::new(AtomicBool::new(false));
    for motion in [false, true] {
        let weak = settings.as_weak();
        let active = active.clone();
        let callback = move || {
            if active.swap(true, Ordering::AcqRel) {
                return;
            }
            let Some(settings) = weak.upgrade() else {
                active.store(false, Ordering::Release);
                return;
            };
            settings.set_apply_error("".into());
            let weak = weak.clone();
            let active = active.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let result = match runtime {
                    Ok(runtime) => runtime.block_on(select(motion)),
                    Err(error) => Err(format!("ファイル選択を開始できません: {error}")),
                };
                let _ = weak.upgrade_in_event_loop(move |settings| {
                    active.store(false, Ordering::Release);
                    match result {
                        Ok(Some(path)) if motion => {
                            settings.set_avatar_motion(path.into());
                            settings.set_motion_playing(true);
                        }
                        Ok(Some(path)) => settings.set_avatar_model(path.into()),
                        Ok(None) => {}
                        Err(error) => settings.set_apply_error(error.into()),
                    }
                });
            });
        };
        if motion {
            settings.on_select_motion(callback);
        } else {
            settings.on_select_model(callback);
        }
    }
}
async fn select(motion: bool) -> Result<Option<String>, String> {
    let extension = if motion { "vrma" } else { "vrm" };
    let response = SelectedFiles::open_file()
        .title(if motion {
            "VRMA 1.0モーションを選択"
        } else {
            "VRM 1.0モデルを選択"
        })
        .multiple(false)
        .filter(
            FileFilter::new(extension)
                .glob(&format!("*.{extension}"))
                .glob(&format!("*.{}", extension.to_uppercase())),
        )
        .send()
        .await
        .and_then(|request| request.response());
    selection(response, extension)
}
fn selection(
    response: Result<SelectedFiles, ashpd::Error>,
    extension: &str,
) -> Result<Option<String>, String> {
    let files = match response {
        Ok(files) => files,
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => return Ok(None),
        Err(error) => return Err(format!("ファイル選択ポータルを利用できません: {error}")),
    };
    let uri = files
        .uris()
        .first()
        .ok_or("ファイルが選択されていません。")?;
    local_path(uri, extension).map(Some)
}
fn local_path(uri: &ashpd::Uri, extension: &str) -> Result<String, String> {
    let path: PathBuf = url::Url::parse(uri.as_str())
        .map_err(|_| "ファイルURIが不正です。")?
        .to_file_path()
        .map_err(|_| "ローカルファイルを選択してください。")?;
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(extension))
        || !path.is_file()
    {
        return Err(format!("存在する .{extension} ファイルを選んでください。"));
    }
    path.canonicalize()
        .map_err(|e| e.to_string())?
        .into_os_string()
        .into_string()
        .map_err(|_| "UTF-8のファイル名を選択してください。".into())
}
