fn main() -> Result<(), Box<dyn std::error::Error>> {
    const PROTO: &str = "proto/host.proto";
    println!("cargo:rerun-if-changed={PROTO}");
    println!("cargo:rerun-if-changed=../../buf.yaml");
    connectrpc_build::Config::new()
        .files(&[PROTO])
        .use_buf()
        .generate_json(true)
        .gate_client_feature(true)
        .include_file("_connectrpc.rs")
        .compile()?;
    Ok(())
}
