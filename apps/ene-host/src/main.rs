use clap::Parser as _;

fn main() {
    let args = ene_host::HostArgs::parse();
    let result = if args.reset_history {
        ene_host::reset_history(&args)
    } else {
        ene_host::run(&args)
    };
    if let Err(error) = result {
        // architecture:allow-stderr: top-level CLI exit may fail before tracing is initialized
        eprintln!("ene-host: {error}");
        std::process::exit(1);
    }
}
