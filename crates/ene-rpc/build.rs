fn main() {
    println!("cargo:rerun-if-changed=proto/host.proto");
    let descriptor =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("host.fds");
    let status = std::process::Command::new("buf")
        .args(["build", "--as-file-descriptor-set", "-o"])
        .arg(&descriptor)
        .arg("proto")
        .status()
        .expect("run buf");
    assert!(status.success(), "buf build failed");
    // Wasm guests share only the message contract; the Connect RPC client and
    // server transports are native.
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|families| families.split(',').any(|family| family == "wasm"));
    if wasm {
        // Extism guests share messages without a browser RPC runtime.
        buffa_build::Config::new()
            .descriptor_set(descriptor.clone())
            .files(&["host.proto"])
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate protobuf messages");
    } else {
        connectrpc_build::Config::new()
            .descriptor_set(descriptor)
            .files(&["host.proto"])
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate Connect services and messages");
    }
}
