//! OS file chooser; late results never resurrect a discarded draft.
use crate::{AppWindow, UiState, application::Application};
use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
use slint::ComponentHandle;
use std::{path::PathBuf, rc::Rc};

pub fn bind(ui: &AppWindow, app: &Rc<Application>) {
    let weak = Rc::downgrade(app);
    let view = ui.as_weak();
    ui.on_file_selection_ready(move |generation, motion, text, present, error| {
        if let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) {
            let Ok(generation) = generation.parse::<u64>() else {
                return;
            };
            let result = if error {
                Err(text.to_string())
            } else {
                Ok(present.then(|| text.to_string()))
            };
            app.selection_result(&ui, generation, motion, result);
        }
    });
    for motion in [false, true] {
        let weak = Rc::downgrade(app);
        let view = ui.as_weak();
        let callback = move || {
            let (Some(app), Some(ui)) = (weak.upgrade(), view.upgrade()) else {
                return;
            };
            let state = ui.global::<UiState>();
            if state.get_picker_active() {
                return;
            }
            state.set_picker_active(true);
            state.set_apply_error("".into());
            let generation = app.settings.borrow().generation.to_string();
            let view = ui.as_weak();
            std::thread::spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(select(motion)),
                    Err(error) => Err(format!("ファイル選択を開始できません: {error}")),
                };
                let _ = view.upgrade_in_event_loop(move |ui| {
                    let (text, present, error) = match result {
                        Ok(Some(path)) => (path, true, false),
                        Ok(None) => (String::new(), false, false),
                        Err(error) => (error, false, true),
                    };
                    ui.invoke_file_selection_ready(
                        generation.into(),
                        motion,
                        text.into(),
                        present,
                        error,
                    );
                });
            });
        };
        if motion {
            ui.global::<UiState>().on_select_motion(callback);
        } else {
            ui.global::<UiState>().on_select_model(callback);
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
