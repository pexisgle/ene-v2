const PROTO: &str = "proto/host.proto";

fn main() {
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|families| families.split(',').any(|family| family == "wasm"));
    // Connect's Wasm runtime imports JavaScript functions unavailable to Extism.
    if wasm {
        buffa_build::Config::new()
            .files(&[PROTO])
            .use_buf()
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate protobuf messages");
    } else {
        connectrpc_build::Config::new()
            .files(&[PROTO])
            .use_buf()
            .generate_json(true)
            .gate_client_feature(true)
            .include_file("_connectrpc.rs")
            .compile()
            .expect("generate Connect services and messages");
    }
}
