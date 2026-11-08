fn main() {
    // connectrpc-build emits no rerun directives in buf mode.
    println!("cargo:rerun-if-changed=proto/host.proto");
    println!("cargo:rerun-if-changed=buf.yaml");
    // Wasm guests share only the message contract; the Connect RPC client and
    // server transports are native.
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|families| families.split(',').any(|family| family == "wasm"));
    if wasm {
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
