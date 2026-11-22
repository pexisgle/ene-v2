fn main() {
    let result = match ene_host::parse(std::env::args_os()) {
        Ok(ene_host::Action::Serve) => ene_host::run(),
        Ok(ene_host::Action::ResetHistory) => ene_host::reset_history(),
        Err(status) => std::process::exit(status),
    };
    if let Err(error) = result {
        // architecture:allow-stderr: top-level CLI exit may fail before tracing is initialized
        eprintln!("ene-host: {error}");
        std::process::exit(1);
    }
}
