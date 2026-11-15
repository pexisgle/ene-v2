fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match ene_host::invocation(std::env::args_os()) {
        Ok(ene_host::Invocation::Host) => ene_host::run(),
        Ok(ene_host::Invocation::Relay(args)) => std::process::exit(ene_host::relay(args)),
        Err(status) => std::process::exit(status),
    }
}
