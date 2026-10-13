fn main() {
    // SAFETY: This build script is single-threaded; configure protoc before codegen.
    unsafe {
        std::env::set_var(
            "PROTOC",
            protoc_bin_vendored::protoc_bin_path().expect("vendored protoc"),
        );
    }
    connectrpc_build::Config::new()
        .files(&["proto/host.proto"])
        .includes(&["proto"])
        .include_file("_connectrpc.rs")
        .compile()
        .expect("generate Connect services and messages");
}
