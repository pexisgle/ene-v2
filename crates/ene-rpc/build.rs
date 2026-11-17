const PROTO: &str = "proto/host.proto";

fn main() {
    // Regenerate when the contract changes: the generator does not emit this
    // itself, so a stale `_connectrpc.rs` would silently outlive a proto edit.
    println!("cargo:rerun-if-changed={PROTO}");
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
