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

use crate::cli::Args;
use crate::clock::SystemClock;
use crate::exec::SystemExec;
use crate::github::GitHub;
use crate::gitlab::GitLab;
use crate::run::{Backends, UsageError};
use crate::slack::Slack;

fn main() -> ExitCode {
    let args = Args::parse();
    let exec = SystemExec::default();
    let backends = Backends {
        github: &GitHub::new(&exec),
        gitlab: &GitLab::new(&exec),
        slack: &Slack::new(&exec),
    };
    let code = run::run(
        &args,
        &backends,
        &exec,
        &SystemClock,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    );
    match code {
        Ok(code) => ExitCode::from(code),
        Err(UsageError(message)) => Args::command()
            .error(ErrorKind::ValueValidation, message)
            .exit(),
    }
}
