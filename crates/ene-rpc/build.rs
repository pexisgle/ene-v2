use std::path::{Path, PathBuf};

const PROTO: &str = "proto/host.proto";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Regenerate when the contract changes: the generator does not emit this
    // itself, so a stale `_connectrpc.rs` would silently outlive a proto edit.
    println!("cargo:rerun-if-changed={PROTO}");
    configure_build_env()?;
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|families| families.split(',').any(|family| family == "wasm"));
    // Connect's Wasm runtime imports JavaScript functions unavailable to Extism.
    if wasm {
        buffa_build::Config::new()
            .files(&[PROTO])
            .use_buf()
            .generate_json(true)
            .include_file("_connectrpc.rs")
            .compile()?;
    } else {
        connectrpc_build::Config::new()
            .files(&[PROTO])
            .use_buf()
            .generate_json(true)
            .gate_client_feature(true)
            .include_file("_connectrpc.rs")
            .compile()?;
    }
    Ok(())
}

fn configure_build_env() -> Result<(), Box<dyn std::error::Error>> {
    // Cargo on Windows may pass mixed slashes in `OUT_DIR`; normalize separators
    // before redirecting temp files there so Low-IL processes do not hit `%TEMP%`
    // (`os error 5`) inside `connectrpc-build::Config::use_buf()`.
    let raw_out_dir = std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?;
    let out_dir: PathBuf = Path::new(&raw_out_dir).components().collect();
    std::fs::create_dir_all(&out_dir)?;

    // SAFETY: `build.rs` is single-threaded here before spawning `buf`, so no
    // other thread can observe a concurrently mutated environment.
    unsafe {
        std::env::set_var("TMP", &out_dir);
    }
    // SAFETY: as above; `build.rs` has not started any thread.
    unsafe {
        std::env::set_var("TEMP", &out_dir);
    }
    // SAFETY: as above; `build.rs` has not started any thread.
    unsafe {
        std::env::set_var("TMPDIR", &out_dir);
    }

    // Cargo's PATH splitting/joining can leave quotes around Windows `PATH`
    // entries when an entry has a stray quote, hiding later tools like `buf`.
    #[cfg(windows)]
    if let Some(raw_path) = std::env::var_os("PATH") {
        let text = raw_path.to_string_lossy();
        if text.contains('"') {
            let cleaned: Vec<&str> = text
                .split(';')
                .map(|entry| entry.trim().trim_matches('"').trim())
                .filter(|entry| !entry.is_empty())
                .collect();
            // SAFETY: `build.rs` is single-threaded here before spawning `buf`.
            unsafe {
                std::env::set_var("PATH", cleaned.join(";"));
            }
        }
    }
    Ok(())
}
