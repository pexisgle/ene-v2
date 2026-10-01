//! In-app model selection; does not depend on a desktop portal or external dialog.
use crate::{FileEntry, FilePickerWindow, SettingsWindow};
use slint::ComponentHandle;
use std::path::{Path, PathBuf};

pub fn bind(settings: &SettingsWindow, picker: &FilePickerWindow) {
    let weak = picker.as_weak();
    picker.on_browse(move |folder| {
        let Some(picker) = weak.upgrade() else { return };
        let generation = picker.get_request_generation().wrapping_add(1);
        picker.set_request_generation(generation);
        picker.set_picker_status("フォルダを読み込んでいます…".into());
        picker.set_entries(slint::ModelRc::default());
        let motion = picker.get_motion();
        let weak = picker.as_weak();
        let folder = folder.to_string();
        std::thread::spawn(move || {
            let listing = list(&folder, motion);
            let _ = weak.upgrade_in_event_loop(move |picker| {
                if picker.get_request_generation() != generation {
                    return;
                }
                match listing {
                    Ok((directory, entries)) => {
                        picker.set_directory(directory.to_string_lossy().into_owned().into());
                        picker.set_parent_directory(
                            directory
                                .parent()
                                .unwrap_or(&directory)
                                .to_string_lossy()
                                .into_owned()
                                .into(),
                        );
                        picker.set_entries(slint::ModelRc::new(slint::VecModel::from(
                            entries
                                .into_iter()
                                .map(|(name, path, directory)| FileEntry {
                                    name: name.into(),
                                    path: path.into(),
                                    directory,
                                })
                                .collect::<Vec<_>>(),
                        )));
                        picker.set_picker_status(
                            "ファイルを選ぶか、下の欄にパスを貼り付けてください。".into(),
                        );
                    }
                    Err(message) => picker.set_picker_status(message.into()),
                }
            });
        });
    });
    let weak = picker.as_weak();
    picker.on_open_entry(move |path, directory| {
        if let Some(picker) = weak.upgrade() {
            if directory {
                picker.invoke_browse(path);
            } else {
                picker.set_selection(path);
            }
        }
    });
    let weak = picker.as_weak();
    let sw = settings.as_weak();
    picker.on_accept(move || {
        let (Some(picker), Some(settings)) = (weak.upgrade(), sw.upgrade()) else {
            return;
        };
        let path = PathBuf::from(picker.get_selection().as_str());
        let extension = if picker.get_motion() { "vrma" } else { "vrm" };
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(extension))
            || !path.is_file()
        {
            picker.set_picker_status(
                format!("存在する .{extension} ファイルを選んでください。").into(),
            );
            return;
        }
        let Ok(path) = path.canonicalize() else {
            picker.set_picker_status("ファイルの場所を確認できません。".into());
            return;
        };
        if picker.get_motion() {
            settings.set_avatar_motion(path.to_string_lossy().into_owned().into());
            settings.set_motion_playing(true);
        } else {
            settings.set_avatar_model(path.to_string_lossy().into_owned().into());
        }
        let _ = picker.hide();
    });
    let weak = picker.as_weak();
    picker.on_cancel(move || {
        if let Some(picker) = weak.upgrade() {
            let _ = picker.hide();
        }
    });
    picker
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    let weak = picker.as_weak();
    let sw = settings.as_weak();
    settings.on_select_model(move || {
        if let (Some(picker), Some(settings)) = (weak.upgrade(), sw.upgrade()) {
            open(&picker, &settings, false);
        }
    });
    let weak = picker.as_weak();
    let sw = settings.as_weak();
    settings.on_select_motion(move || {
        if let (Some(picker), Some(settings)) = (weak.upgrade(), sw.upgrade()) {
            open(&picker, &settings, true);
        }
    });
}

fn open(picker: &FilePickerWindow, settings: &SettingsWindow, motion: bool) {
    picker
        .global::<crate::Palette>()
        .set_color_scheme(settings.global::<crate::Palette>().get_color_scheme());
    picker.set_motion(motion);
    picker.set_picker_title(
        if motion {
            "VRMA 1.0モーションを選択"
        } else {
            "VRM 1.0モデルを選択"
        }
        .into(),
    );
    let selection = if motion {
        settings.get_avatar_motion()
    } else {
        settings.get_avatar_model()
    };
    picker.set_selection(selection.clone());
    let path = Path::new(selection.as_str());
    let directory = path
        .parent()
        .filter(|p| p.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(error) = picker.show() {
        settings.set_avatar_status(format!("ファイル選択画面を開けません: {error}").into());
        return;
    }
    picker.invoke_browse(directory.to_string_lossy().into_owned().into());
}

type Entry = (String, String, bool);
fn list(folder: &str, motion: bool) -> Result<(PathBuf, Vec<Entry>), String> {
    let folder = Path::new(folder)
        .canonicalize()
        .map_err(|_| "フォルダの場所を確認できません。".to_string())?;
    let entries = std::fs::read_dir(&folder).map_err(|_| {
        "フォルダを読み込めません。場所とアクセス権を確認してください。".to_string()
    })?;
    let extension = if motion { "vrma" } else { "vrm" };
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "フォルダ内の項目を読み込めません。".to_string())?;
        let path = entry.path();
        let directory = path.is_dir();
        if !directory
            && (!path.is_file()
                || !path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case(extension)))
        {
            continue;
        }
        let (Some(name), Some(path)) = (
            entry.file_name().to_str().map(str::to_owned),
            path.to_str().map(str::to_owned),
        ) else {
            continue;
        };
        result.push((
            if directory { format!("{name}/") } else { name },
            path,
            directory,
        ));
    }
    result.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    Ok((folder, result))
}
