//! The binary's command line: the host takes no arguments.
//!
//! It is configured by its environment and the files in its runtime directory,
//! so an argument is a mistake the operator is told about rather than an option
//! the host honours.
use std::ffi::OsString;

/// What the host tells an operator who asks it for help or passes it something
/// it does not take. The endpoint and Owner token are published in the runtime
/// directory's connection.json.
pub const USAGE: &str = "ene-host takes no arguments.\n\
The endpoint and Owner token are published in the runtime directory's connection.json.\n\
ENE_HOST_STATE_DIR overrides the SQLite/rig directory.\n\
ENE_HOST_RUNTIME_DIR overrides the discovery/lock directory.";

/// Parses the process's arguments. `Err` carries the exit status to leave with,
/// after the usage text is already printed: help succeeds, anything else is a
/// misuse.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<(), i32> {
    let mut arguments = arguments.into_iter();
    arguments.next();
    match arguments.next() {
        None => Ok(()),
        Some(argument) if argument == "-h" || argument == "--help" => {
            println!("{USAGE}");
            Err(0)
        }
        Some(_) => {
            eprintln!("{USAGE}");
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(listed: &[&str]) -> Vec<OsString> {
        std::iter::once("ene-host")
            .chain(listed.iter().copied())
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn no_arguments_serves_the_host() {
        assert_eq!(parse(arguments(&[])), Ok(()));
    }

    #[test]
    fn help_is_a_success_with_no_run() {
        for argument in ["-h", "--help"] {
            assert_eq!(parse(arguments(&[argument])), Err(0), "{argument} exits 0");
        }
    }

    #[test]
    fn an_argument_the_host_does_not_take_is_a_failure() {
        for listed in [vec!["--bogus"], vec!["a", "b"], vec!["acp-relay"]] {
            assert_eq!(parse(arguments(&listed)), Err(1), "{listed:?} exits 1");
        }
    }
}
