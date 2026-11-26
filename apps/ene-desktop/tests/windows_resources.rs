#![cfg(windows)]

use object::read::pe::PeFile64;

#[test]
fn desktop_executable_embeds_the_tray_icon_and_gpui_manifest()
-> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(env!("CARGO_BIN_EXE_ene-desktop"))?;
    let file = PeFile64::parse(bytes.as_slice())?;
    let resource = file
        .data_directories()
        .resource_directory(file.data(), &file.section_table())?
        .ok_or("desktop executable has no resource directory")?;
    let root = resource.root()?;
    for (kind, label) in [(14, "tray icon group"), (24, "GPUI manifest")] {
        let type_entry = root
            .entries
            .iter()
            .find(|entry| entry.name_or_id().id() == Some(kind))
            .ok_or(format!("missing {label} resource type"))?;
        let table = type_entry
            .data(resource)?
            .table()
            .ok_or(format!("{label} has no resource ID table"))?;
        assert!(
            table
                .entries
                .iter()
                .any(|entry| entry.name_or_id().id() == Some(1)),
            "missing {label} with resource ID 1"
        );
    }
    Ok(())
}
