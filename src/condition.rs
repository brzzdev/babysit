use crate::envelope::Summary;
use crate::forge::{Colour, Merge, TargetKind, TargetState};
use crate::rule;

const ABSENT_CI: [(&str, bool); 2] = [("false", false), ("true", true)];

const COLOURS: [(&str, Colour); 3] = [
    ("amber", Colour::Amber),
    ("green", Colour::Green),
    ("red", Colour::Red),
];

const MERGES: [(&str, Merge); 3] = [
    ("behind", Merge::Behind),
    ("clean", Merge::Clean),
    ("conflicted", Merge::Conflicted),
];

const STATES: [(&str, TargetState); 3] = [
    ("closed", TargetState::Closed),
    ("merged", TargetState::Merged),
    ("open", TargetState::Open),
];

/// A caller's test on a target's current summary: terms that must all hold.
#[derive(Clone, Debug)]
pub struct Condition {
    terms: Vec<Term>,
}

/// One `field=value[,value…]` term, holding when the summary's field is any value.
#[derive(Clone, Debug)]
enum Term {
    AbsentCi(Vec<bool>),
    Colour(Vec<Colour>),
    Merge(Vec<Merge>),
    State(Vec<TargetState>),
    UnresolvedThreads(Vec<usize>),
}

pub fn parse(given: &str) -> Result<Condition, String> {
    let mut names = Vec::new();
    let mut terms = Vec::new();
    for word in rule::split(given)? {
        let (name, term) = term(word)?;
        if names.contains(&name) {
            return Err(format!("`{name}` appears twice in one condition"));
        }
        names.push(name);
        terms.push(term);
    }
    if terms.is_empty() {
        return Err("a condition needs a `field=value` term".to_owned());
    }
    Ok(Condition { terms })
}

fn term(word: &str) -> Result<(&str, Term), String> {
    let Some((name, given)) = word.split_once('=') else {
        return Err(format!(
            "`{word}` is not a `field=value` term; a condition takes no kind pattern"
        ));
    };
    if name.contains('"') {
        return Err(rule::outside_a_value(word));
    }
    let values = rule::values(word, given)?;
    let values = values.iter().map(String::as_str);
    let term = match name {
        "absent_ci" => Term::AbsentCi(words(values, &ABSENT_CI, name)?),
        "colour" => Term::Colour(words(values, &COLOURS, name)?),
        "merge" => Term::Merge(words(values, &MERGES, name)?),
        "state" => Term::State(words(values, &STATES, name)?),
        "unresolved_threads" => {
            Term::UnresolvedThreads(values.map(count).collect::<Result<_, _>>()?)
        }
        _ => {
            return Err(format!(
                "`{name}` is not a field: use absent_ci, colour, merge, state or \
                 unresolved_threads"
            ));
        }
    };
    Ok((name, term))
}

fn count(value: &str) -> Result<usize, String> {
    let invalid = || format!("`{value}` is not a whole number");
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    value.parse().map_err(|_| invalid())
}

fn words<'a, T: Copy>(
    values: impl Iterator<Item = &'a str>,
    known: &[(&str, T)],
    field: &str,
) -> Result<Vec<T>, String> {
    values
        .map(|value| {
            known
                .iter()
                .find(|(word, _)| *word == value)
                .map(|(_, meant)| *meant)
                .ok_or_else(|| {
                    let known: Vec<_> = known.iter().map(|(word, _)| *word).collect();
                    format!("`{value}` is not a {field}: use {}", known.join(", "))
                })
        })
        .collect()
}

/// Whether a target of `kind` can meet at least one of `conditions`, or there are none.
pub fn fit(conditions: &[Condition], kind: TargetKind) -> bool {
    conditions.is_empty()
        || kind == TargetKind::ChangeRequest
        || conditions.iter().any(|condition| {
            condition
                .terms
                .iter()
                .all(|term| matches!(term, Term::State(_)))
        })
}

/// Whether `summary` passes at least one of `conditions`.
pub fn met(conditions: &[Condition], summary: &Summary) -> bool {
    conditions
        .iter()
        .any(|condition| condition.terms.iter().all(|term| term.holds(summary)))
}

impl Term {
    fn holds(&self, summary: &Summary) -> bool {
        let change = summary.change.as_ref();
        match self {
            Self::AbsentCi(values) => {
                change.is_some_and(|change| values.contains(&change.absent_ci))
            }
            Self::Colour(colours) => change
                .and_then(|change| change.colour)
                .is_some_and(|colour| colours.contains(&colour)),
            Self::Merge(merges) => change
                .and_then(|change| change.merge)
                .is_some_and(|merge| merges.contains(&merge)),
            Self::State(states) => states.contains(&summary.state),
            Self::UnresolvedThreads(counts) => {
                change.is_some_and(|change| counts.contains(&change.unresolved_threads))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_specs_examples() {
        for given in [
            "colour=green,red,amber",
            "absent_ci=true",
            "state=closed",
            "state=open merge=clean unresolved_threads=0",
            r#"colour="green""#,
        ] {
            assert!(parse(given).is_ok(), "{given}");
        }
    }

    #[test]
    fn rejects_every_usage_error_naming_the_culprit() {
        for (given, culprit) in [
            ("", "a `field=value` term"),
            ("ci.settled colour=green", "no kind pattern"),
            ("actor=@me", "`actor`"),
            ("colour=blue", "`blue`"),
            ("state=approved", "`approved`"),
            ("merge=dirty", "`dirty`"),
            ("absent_ci=yes", "`yes`"),
            ("unresolved_threads=-1", "`-1`"),
            ("unresolved_threads=", "`` is not a whole number"),
            ("colour=green colour=red", "`colour` appears twice"),
            ("colour^=green", "`colour^`"),
            (r#""colour"=green"#, "outside a value"),
            (r#"colour="green"#, "unterminated"),
        ] {
            let error = parse(given).expect_err(given);
            assert!(error.contains(culprit), "{given}: {error}");
        }
    }
}
