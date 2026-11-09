mod cli;
mod clock;
mod condition;
mod diff;
mod envelope;
mod exec;
mod forge;
mod github;
mod gitlab;
mod graphql;
mod out_file;
mod reference;
mod response;
mod rule;
mod run;
mod slack;
mod snapshot_file;
mod target;

use std::io::{self, Write};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches};

use crate::cli::{Args, Cli, Command};
use crate::clock::SystemClock;
use crate::exec::SystemExec;
use crate::github::GitHub;
use crate::gitlab::GitLab;
use crate::out_file::OutFile;
use crate::run::{Backends, UsageError};
use crate::slack::Slack;

fn main() -> ExitCode {
    let mut command = Cli::command();
    let matches = command.get_matches_mut();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let code = match cli.command {
        Some(Command::Forget { snapshot }) => run::forget(&snapshot, &mut io::stdout().lock()),
        None => watch(
            &cli.run
                .expect("clap requires the run arguments when no subcommand is given"),
        ),
    };
    let message = match code {
        Ok(code) => return ExitCode::from(code),
        Err(UsageError(message)) => message,
    };
    let command = match matches.subcommand_name() {
        Some(name) => command
            .find_subcommand_mut(name)
            .expect("clap matched this subcommand"),
        None => &mut command,
    };
    command.error(ErrorKind::ValueValidation, message).exit()
}

fn watch(args: &Args) -> Result<u8, UsageError> {
    let exec = SystemExec::default();
    let backends = Backends {
        github: &GitHub::new(&exec),
        gitlab: &GitLab::new(&exec),
        slack: &Slack::new(&exec),
    };
    let mut stdout: Box<dyn Write> = match &args.out {
        Some(path) => Box::new(OutFile::new(path)),
        None => Box::new(io::stdout().lock()),
    };
    run::run(
        args,
        &backends,
        &exec,
        &SystemClock,
        &mut stdout,
        &mut io::stderr().lock(),
    )
}
