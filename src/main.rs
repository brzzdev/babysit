mod cli;
mod clock;
mod diff;
mod envelope;
mod exec;
mod forge;
mod github;
mod gitlab;
mod graphql;
mod reference;
mod response;
mod rule;
mod run;
mod slack;
mod snapshot_file;
mod target;

use std::io;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

use crate::cli::{Args, Cli, Command};
use crate::clock::SystemClock;
use crate::exec::SystemExec;
use crate::github::GitHub;
use crate::gitlab::GitLab;
use crate::run::{Backends, UsageError};
use crate::slack::Slack;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (code, subcommand) = match cli.command {
        Some(Command::Forget { snapshot }) => (
            run::forget(&snapshot, &mut io::stdout().lock()),
            Some("forget"),
        ),
        None => (
            watch(
                &cli.run
                    .expect("clap requires the run arguments when no subcommand is given"),
            ),
            None,
        ),
    };
    match code {
        Ok(code) => ExitCode::from(code),
        Err(UsageError(message)) => {
            let mut command = Cli::command();
            command.build();
            let command = match subcommand {
                Some(name) => command
                    .find_subcommand_mut(name)
                    .expect("the subcommand that ran is one clap parsed"),
                None => &mut command,
            };
            command.error(ErrorKind::ValueValidation, message).exit()
        }
    }
}

fn watch(args: &Args) -> Result<u8, UsageError> {
    let exec = SystemExec::default();
    let backends = Backends {
        github: &GitHub::new(&exec),
        gitlab: &GitLab::new(&exec),
        slack: &Slack::new(&exec),
    };
    run::run(
        args,
        &backends,
        &exec,
        &SystemClock,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}
