//! The binary's command line: the host itself, and the relay it runs inside a
//! network-isolated ACP sandbox.
//!
//! One definition covers both modes, so the arguments a launch passes to itself
//! and the arguments an operator types are checked by the same parser.
use crate::host::work::acp_bridge::RelayArgs;
use clap::{Parser, Subcommand};
use std::ffi::OsString;

/// What the host tells an operator who asks it for help or passes it something
/// it does not take. The host is configured by its environment and the files in
/// its runtime directory, so it has no options of its own.
pub const USAGE: &str = "ene-host takes no arguments.\n\
The endpoint and Owner token are published in the runtime directory's connection.json.\n\
ENE_HOST_STATE_DIR overrides the SQLite/rig directory.\n\
ENE_HOST_RUNTIME_DIR overrides the discovery/lock directory.";

#[derive(Parser)]
#[command(name = "ene-host", about = USAGE, help_template = "{about}")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    // Serves the sandbox's loopback proxy and runs the agent inside it.
    #[command(name = "acp-relay")]
    AcpRelay(RelayArgs),
}

/// What this process was started to do.
#[derive(Debug)]
pub enum Invocation {
    /// Serve the host API.
    Host,
    /// Run the sandbox relay and exit with the agent's own status.
    Relay(RelayArgs),
}

/// Parses the process's arguments. `Err` carries the exit status to leave with,
/// after whatever the parser already printed.
///
/// The host takes no arguments, so an unknown one is the usage text on stderr
/// and status 1, as it has always been; the relay keeps clap's own help and
/// status codes.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Invocation, i32> {
    let arguments: Vec<OsString> = arguments.into_iter().collect();
    let relay = arguments.get(1).is_some_and(|first| first == "acp-relay");
    match Cli::try_parse_from(arguments) {
        Ok(cli) => Ok(match cli.command {
            Some(Command::AcpRelay(args)) => Invocation::Relay(args),
            None => Invocation::Host,
        }),
        Err(error) => {
            use clap::error::ErrorKind;
            match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    let _ = error.print();
                    Err(0)
                }
                // A relay argument error is the relay's to report, in clap's
                // own words and status.
                _ if relay => {
                    let _ = error.print();
                    Err(error.exit_code())
                }
                _ => {
                    eprintln!("{USAGE}");
                    Err(1)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a launch passes itself, exactly as `acp::plan` writes it.
    fn relay_arguments() -> Vec<OsString> {
        [
            "ene-host",
            "acp-relay",
            "--socket",
            "/run/ene/proxy.sock",
            "--listen",
            "127.0.0.1:3128",
            "--",
            "agent",
            "--stdio",
        ]
        .map(OsString::from)
        .to_vec()
    }

    #[test]
    fn no_arguments_serves_the_host() {
        assert!(matches!(
            parse([OsString::from("ene-host")]),
            Ok(Invocation::Host)
        ));
    }

    #[test]
    fn a_launch_argument_vector_runs_the_relay() {
        assert!(matches!(parse(relay_arguments()), Ok(Invocation::Relay(_))));
    }

    #[test]
    fn help_is_a_success_with_no_invocation() {
        for argument in ["-h", "--help"] {
            assert_eq!(
                parse([OsString::from("ene-host"), OsString::from(argument)]).err(),
                Some(0),
                "{argument} exits 0"
            );
        }
    }

    #[test]
    fn an_argument_the_host_does_not_take_is_a_failure() {
        for arguments in [vec!["--bogus"], vec!["a", "b"]] {
            let parsed = parse(
                std::iter::once("ene-host")
                    .chain(arguments.iter().copied())
                    .map(OsString::from),
            );
            assert_eq!(parsed.err(), Some(1), "{arguments:?} exits 1");
        }
    }

    #[test]
    fn the_relay_keeps_clap_status_codes() {
        // A missing required argument is clap's usage error, not the host's.
        assert_eq!(
            parse([OsString::from("ene-host"), OsString::from("acp-relay")]).err(),
            Some(2)
        );
        assert_eq!(
            parse([
                OsString::from("ene-host"),
                OsString::from("acp-relay"),
                OsString::from("--help"),
            ])
            .err(),
            Some(0)
        );
    }
}
