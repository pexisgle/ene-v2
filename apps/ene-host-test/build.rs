fn main() {
    println!("cargo:rerun-if-changed=proto/chat/v1/chat.proto");
    println!("cargo:rerun-if-changed=proto/settings/v1/settings.proto");
    connectrpc_build::Config::new()
        .files(&[
            "proto/chat/v1/chat.proto",
            "proto/settings/v1/settings.proto",
        ])
        .use_buf()
        .include_file("_connectrpc.rs")
        .compile()
        .unwrap();
}
