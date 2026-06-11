//! CLI surface — clap subcommands for the `tiygate` binary.
//!
//! Subcommands (stage 4):
//! * `run` (default) — start the gateway.
//! * `migrate` — apply pending DB migrations and exit.
//! * `migrate-status` — print the applied migrations and exit.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Top-level CLI arguments.
#[derive(Debug, Parser)]
#[command(name = "tiygate", about = "TiyGate AI Gateway", version)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Args {
    /// Parse from `std::env::args_os`, exiting on error. Equivalent to
    /// `Args::parse()` but tolerant of test environments.
    pub fn parse_or_exit() -> Self {
        Self::try_parse_from(std::env::args_os()).unwrap_or_else(|e| {
            // clap's `parse` prints + exits; replicate that with
            // ExitCode::from(2) so the CI runner sees a non-zero
            // status. We do not call `clap::Command::exit` because
            // that pulls in a TTY-aware code path.
            eprintln!("{e}");
            std::process::exit(2);
        })
    }

    /// Parse from an explicit iterator. Tests use this to avoid
    /// mutating process-global state.
    pub fn try_parse_from<I, T>(it: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(it)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the gateway (default).
    Run(RunArgs),
    /// Apply pending schema migrations and exit.
    Migrate,
    /// Print the applied schema migrations and exit.
    MigrateStatus,
}

/// Arguments for the `run` subcommand.
#[derive(Debug, Parser, Default, Clone)]
pub struct RunArgs {
    /// Override the listen address (TIYGATE_LISTEN_ADDR).
    #[arg(long, env = "TIYGATE_LISTEN_ADDR")]
    pub listen_addr: Option<String>,
    /// Override the deployment mode (TIYGATE_MODE).
    #[arg(long, env = "TIYGATE_MODE")]
    pub mode: Option<String>,
}

impl RunArgs {
    /// No-op helper to avoid a `Default`-only lint warning when the
    /// struct has all-optional fields.
    pub fn is_default(&self) -> bool {
        self.listen_addr.is_none() && self.mode.is_none()
    }
}

impl From<RunArgs> for ExitCode {
    fn from(_: RunArgs) -> Self {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_is_run() {
        let args = Args::try_parse_from(["tiygate"]).expect("parse");
        assert!(matches!(args.command, None | Some(Command::Run(_))));
    }

    #[test]
    fn parse_migrate_subcommand() {
        let args = Args::try_parse_from(["tiygate", "migrate"]).expect("parse");
        assert!(matches!(args.command, Some(Command::Migrate)));
    }

    #[test]
    fn parse_migrate_status_subcommand() {
        let args = Args::try_parse_from(["tiygate", "migrate-status"]).expect("parse");
        assert!(matches!(args.command, Some(Command::MigrateStatus)));
    }

    #[test]
    fn parse_run_with_overrides() {
        let args = Args::try_parse_from([
            "tiygate",
            "run",
            "--listen-addr",
            "0.0.0.0:9000",
            "--mode",
            "proxy",
        ])
        .expect("parse");
        match args.command {
            Some(Command::Run(r)) => {
                assert_eq!(r.listen_addr.as_deref(), Some("0.0.0.0:9000"));
                assert_eq!(r.mode.as_deref(), Some("proxy"));
            }
            _ => panic!("expected Run"),
        }
    }
}
