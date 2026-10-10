#[cfg(not(feature = "rpc"))]
fn main() {}

#[cfg(feature = "rpc")]
fn main() {
    println!("cargo:rerun-if-changed=proto/host.proto");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let descriptor = out.join("ene-descriptor.bin");
    let status =
        std::process::Command::new(protoc_bin_vendored::protoc_bin_path().expect("protoc"))
            .arg("-Iproto")
            .arg(format!("--descriptor_set_out={}", descriptor.display()))
            .arg("--include_imports")
            .arg("--include_source_info")
            .arg("proto/host.proto")
            .status()
            .expect("run vendored protoc");
    assert!(status.success(), "compile host.proto");
    connectrpc_build::Config::new()
        // The descriptor is produced by this build, not an input to watch.
        .emit_rerun_directives(false)
        .files(&["host.proto"])
        .descriptor_set(descriptor)
        .include_file("_connectrpc.rs")
        .compile()
        .expect("generate Connect services and messages");
}
