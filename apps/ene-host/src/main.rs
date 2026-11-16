fn main() {
    match ene_host::parse(std::env::args_os()) {
        Ok(()) => {
            // Printed through Display, so the sentence about a refused directory
            // is the whole message instead of an `Error: Custom { ... }` dump.
            if let Err(error) = ene_host::run() {
                eprintln!("ene-host: {error}");
                std::process::exit(1);
            }
        }
        Err(status) => std::process::exit(status),
    }
}
