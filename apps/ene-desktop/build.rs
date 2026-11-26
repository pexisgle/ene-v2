//! Links the Windows window and tray icon with the native resource compiler.

fn main() -> Result<(), embed_resource::CompilationResult> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let directory = "resources/windows";
    let resource = "resources/windows/ene.rc";
    println!("cargo:rerun-if-changed={resource}");
    println!("cargo:rerun-if-changed=resources/windows/ene.ico");

    embed_resource::compile_for(
        resource,
        ["ene-desktop"],
        embed_resource::ParamsIncludeDirs([directory]),
    )
    .manifest_required()
}
