use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::reference::{self, Name};
use crate::rule::{self, Rule};

#[derive(Debug, Parser)]
#[command(name = "babysit", version, about, long_about = None)]
pub struct Args {
    /// Wake only on events matching at least one such rule, such as `'thread.* actor=@human'`;
    /// repeatable
    #[arg(long, value_name = "RULE", value_parser = rule::parse)]
    pub on: Vec<Rule>,

    /// Never wake on an event matching this rule, even one an `--on` rule matches; repeatable
    #[arg(long, value_name = "RULE", value_parser = rule::parse)]
    pub ignore: Vec<Rule>,

    /// The pull requests to watch, each as its GitHub web URL or `#<number>` inside a checkout
    #[arg(value_name = "TARGET", required = true, value_parser = reference::parse_name)]
    pub targets: Vec<Name>,

    /// The file that keeps what babysit has seen between runs
    #[arg(long, value_name = "PATH")]
    pub snapshot: PathBuf,

    /// Time between polls, such as `30s`, `10m` or `1h`
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_interval)]
    pub interval: Duration,

    /// Wall-clock cap on the run; `0` polls once
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub timeout: Option<Duration>,
}

fn parse_duration(given: &str) -> Result<Duration, String> {
    if given == "0" {
        return Ok(Duration::ZERO);
    }
    let invalid = || format!("`{given}` is not a whole number of `s`, `m` or `h`");
    let unit = match given.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3_600,
        _ => return Err(invalid()),
    };
    let number = &given[..given.len() - 1];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let seconds = number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(unit));
    seconds.map(Duration::from_secs).ok_or_else(invalid)
}

fn parse_interval(given: &str) -> Result<Duration, String> {
    let interval = parse_duration(given)?;
    if interval.is_zero() {
        return Err("the interval must be longer than zero".to_owned());
    }
    Ok(interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_durations_in_seconds_minutes_and_hours() {
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("45s"), Ok(Duration::from_secs(45)));
        assert_eq!(parse_duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7_200)));
    }

    #[test]
    fn rejects_malformed_durations() {
        for given in [
            "",
            "30",
            "s",
            "1.5m",
            "-1s",
            "+1s",
            "10ms",
            "1d",
            "99999999999999999999h",
        ] {
            assert!(parse_duration(given).is_err(), "{given}");
        }
        assert!(parse_interval("0s").is_err());
    }

    #[test]
    fn rejects_malformed_arguments_before_running() {
        for args in [
            &["babysit", "https://github.com/o/r/pull/1"][..],
            &["babysit", "--snapshot", "s.json"],
            &[
                "babysit",
                "--snapshot",
                "s.json",
                "https://github.com/o/r/issues/1",
            ],
            &[
                "babysit",
                "--snapshot",
                "s.json",
                "--interval",
                "5",
                "https://github.com/o/r/pull/1",
            ],
            &[
                "babysit",
                "--snapshot",
                "s.json",
                "--on",
                "x",
                "https://github.com/o/r/pull/1",
            ],
        ] {
            assert!(Args::try_parse_from(args).is_err(), "{args:?}");
        }
    }
}
