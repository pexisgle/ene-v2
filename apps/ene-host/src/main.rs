fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(code) = ene_host::relay() {
        std::process::exit(code);
    }
    ene_host::run()
}
