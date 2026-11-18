fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match ene_host::parse(std::env::args_os()) {
        Ok(()) => ene_host::run(),
        Err(status) => std::process::exit(status),
    }
}
