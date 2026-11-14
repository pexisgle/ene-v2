const PROTO: &str = "proto/host.proto";

fn main() {
    println!("cargo:rerun-if-changed={PROTO}");
    // Wasm guests share only the message contract; the Connect RPC transports
    // are native. Both generators must produce the same message types, so the
    // message options here are set explicitly on both sides rather than left
    // to defaults: `generate_json` defaults to true in connectrpc-build and
    // false in buffa-build.
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|families| families.split(',').any(|family| family == "wasm"));
    if wasm {
        // Extism guests share messages without a browser RPC runtime.
        buffa_build::Config::new()
            .files(&[PROTO])
            .use_buf()
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate protobuf messages");
    } else {
        // The same messages, plus the Connect RPC service traits and client.
        connectrpc_build::Config::new()
            .files(&[PROTO])
            .use_buf()
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate Connect services and messages");
    }
}
