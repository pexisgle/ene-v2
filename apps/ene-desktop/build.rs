//! Embeds the Windows application manifest into the client binary.
//!
//! GPUI imports `comctl32!TaskDialogIndirect`, which only resolves against the
//! Common-Controls version 6 side-by-side assembly. A process that does not ask
//! for it in its manifest loads the version 5 stub and dies at load time with
//! `STATUS_ENTRYPOINT_NOT_FOUND`, before `main`. GPUI ships a manifest of its
//! own, but a dependency's link arguments do not reach this crate's binary, so
//! the client embeds it here.

fn main() {
    println!("cargo:rerun-if-changed=ene-desktop.manifest");
    println!("cargo:rerun-if-changed=ene-desktop.rc");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    embed_resource::compile("ene-desktop.rc", embed_resource::NONE)
        .manifest_required()
        .expect("the application manifest is embedded");
}
