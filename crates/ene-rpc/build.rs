fn main() {
    // SAFETY: This build script is single-threaded; configure protoc before codegen.
    unsafe {
        std::env::set_var(
            "PROTOC",
            protoc_bin_vendored::protoc_bin_path().expect("vendored protoc"),
        );
    }
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("wasm") {
        // Extism guests share messages without a browser RPC runtime.
        buffa_build::Config::new()
            .files(&["proto/host.proto"])
            .includes(&["proto"])
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate protobuf messages");
    } else {
        connectrpc_build::Config::new()
            .files(&["proto/host.proto"])
            .includes(&["proto"])
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate Connect services and messages");
    }
}
