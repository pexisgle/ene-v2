fn main() {
    // connectrpc-build emits no rerun directives in buf mode.
    println!("cargo:rerun-if-changed=proto/host.proto");
    println!("cargo:rerun-if-changed=buf.yaml");
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("wasm") {
        // Extism guests share messages without a browser RPC runtime.
        buffa_build::Config::new()
            .use_buf()
            .files(&["proto/host.proto"])
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate protobuf messages");
    } else {
        connectrpc_build::Config::new()
            .use_buf()
            .files(&["proto/host.proto"])
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate Connect services and messages");
    }
}
