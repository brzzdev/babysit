use crate::diff::{Detail, Event};
use crate::forge::Account;

const COLOURS: [&str; 3] = ["amber", "green", "red"];

/// Every event kind in v1 with the fields it carries, including kinds no backend reports yet.
const KINDS: [Kind; 15] = [
    ("ci.settled", &[Field::Colour]),
    ("comment.created", &[Field::Actor]),
    ("comment.edited", &[Field::Actor]),
    ("head.pushed", &[Field::Actor]),
    ("merge.behind", &[]),
    ("merge.conflicted", &[]),
    ("review.submitted", &[Field::Actor, Field::State]),
    ("review.withdrawn", &[Field::Actor, Field::Subject]),
    ("state.closed", &[Field::Actor]),
    ("state.merged", &[Field::Actor]),
    ("state.reopened", &[Field::Actor]),
    ("thread.opened", &[Field::Actor]),
    ("thread.replied", &[Field::Actor]),
    ("thread.resolved", &[Field::Actor]),
    ("thread.unresolved", &[Field::Actor]),
];

const STATES: [&str; 3] = ["approved", "changes_requested", "commented"];

type Kind = (&'static str, &'static [Field]);

/// A caller's condition on events: the kinds its pattern covers, then terms that must all hold.
#[derive(Clone, Debug)]
pub struct Rule {
    kinds: Vec<Kind>,
    terms: Vec<Term>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Actor,
    Colour,
    State,
    Subject,
}

/// One `field=value[,value…]` term, holding when the event's field matches any value.
#[derive(Clone, Debug)]
enum Term {
    Accounts(Field, Vec<Selector>),
    Words(Field, Vec<&'static str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selector {
    Bot,
    Human,
    Login(String),
    Me,
}

pub fn parse(given: &str) -> Result<Rule, String> {
    let mut words = given.split_whitespace().peekable();
    let pattern = words.next_if(|word| !word.contains('='));
    let kinds = kinds(pattern)?;
    let mut terms: Vec<Term> = Vec::new();
    for word in words {
        let term = term(word)?;
        let field = term.field();
        if terms.iter().any(|seen| seen.field() == field) {
            return Err(format!("`{}` appears twice in one rule", field.name()));
        }
        if let Some(pattern) = pattern
            && !kinds.iter().any(|(_, fields)| fields.contains(&field))
        {
            return Err(format!("no `{pattern}` event carries `{}`", field.name()));
        }
        terms.push(term);
    }
    if pattern.is_none() && terms.is_empty() {
        return Err("a rule needs a kind pattern or a `field=value` term".to_owned());
    }
    Ok(Rule { kinds, terms })
}

/// The kinds an exact or `noun.*` pattern covers; every kind when there is no pattern.
fn kinds(pattern: Option<&str>) -> Result<Vec<Kind>, String> {
    let Some(pattern) = pattern else {
        return Ok(KINDS.to_vec());
    };
    let covered: Vec<Kind> = KINDS
        .into_iter()
        .filter(|(kind, _)| match pattern.strip_suffix(".*") {
            Some(noun) => kind.split_once('.').is_some_and(|(seen, _)| seen == noun),
            None => *kind == pattern,
        })
        .collect();
    if covered.is_empty() {
        return Err(format!(
            "`{pattern}` is not an event kind or a `field=value` term"
        ));
    }
    Ok(covered)
}

fn term(word: &str) -> Result<Term, String> {
    let (name, values) = word.split_once('=').ok_or_else(|| {
        format!("`{word}` is not a `field=value` term; a kind pattern comes first")
    })?;
    let field = match name {
        "actor" => Field::Actor,
        "colour" => Field::Colour,
        "state" => Field::State,
        "subject" => Field::Subject,
        _ => {
            return Err(format!(
                "`{name}` is not a field: use actor, colour, state or subject"
            ));
        }
    };
    let values = values.split(',');
    Ok(match field {
        Field::Actor | Field::Subject => {
            Term::Accounts(field, values.map(selector).collect::<Result<_, _>>()?)
        }
        Field::Colour => Term::Words(field, words(values, &COLOURS, field)?),
        Field::State => Term::Words(field, words(values, &STATES, field)?),
    })
}

fn selector(value: &str) -> Result<Selector, String> {
    match value {
        "" => Err("an account value is empty".to_owned()),
        "@bot" => Ok(Selector::Bot),
        "@human" => Ok(Selector::Human),
        "@me" => Ok(Selector::Me),
        _ if value.starts_with('@') => Err(format!(
            "`{value}` is not a selector: use @bot, @human or @me"
        )),
        _ if value.contains('=') => Err(format!("`{value}` is not a login")),
        _ => Ok(Selector::Login(value.to_owned())),
    }
}

fn words<'a>(
    values: impl Iterator<Item = &'a str>,
    known: &[&'static str],
    field: Field,
) -> Result<Vec<&'static str>, String> {
    values
        .map(|value| {
            known
                .iter()
                .copied()
                .find(|word| *word == value)
                .ok_or_else(|| {
                    format!(
                        "`{value}` is not a {}: use {}",
                        field.name(),
                        known.join(", ")
                    )
                })
        })
        .collect()
}

/// Whether `event` wakes the run: it matches at least one `on` rule (or there are none) and no
/// `ignore` rule. `me` is the viewer's account id on the event's forge.
pub fn wakes(on: &[Rule], ignore: &[Rule], event: &Event, me: Option<&str>) -> bool {
    let matches = |rule: &Rule| rule.matches(event, me);
    (on.is_empty() || on.iter().any(matches)) && !ignore.iter().any(matches)
}

/// Whether any of `rules` names `@me`.
pub fn names_me<'a>(rules: impl IntoIterator<Item = &'a Rule>) -> bool {
    rules.into_iter().flat_map(|rule| &rule.terms).any(
        |term| matches!(term, Term::Accounts(_, selectors) if selectors.contains(&Selector::Me)),
    )
}

impl Rule {
    fn matches(&self, event: &Event, me: Option<&str>) -> bool {
        let kind = event.detail.kind();
        self.kinds.iter().any(|(covered, _)| *covered == kind)
            && self.terms.iter().all(|term| term.matches(event, me))
    }
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::Actor => "actor",
            Self::Colour => "colour",
            Self::State => "state",
            Self::Subject => "subject",
        }
    }
}

impl Term {
    fn field(&self) -> Field {
        match self {
            Self::Accounts(field, _) | Self::Words(field, _) => *field,
        }
    }

    fn matches(&self, event: &Event, me: Option<&str>) -> bool {
        match self {
            Self::Accounts(field, selectors) => account(event, *field).is_some_and(|account| {
                selectors
                    .iter()
                    .any(|selector| selector.matches(account, me))
            }),
            Self::Words(field, words) => {
                word(&event.detail, *field).is_some_and(|word| words.contains(&word.as_str()))
            }
        }
    }
}

impl Selector {
    fn matches(&self, account: &Account, me: Option<&str>) -> bool {
        match self {
            Self::Bot => account.bot == Some(true),
            Self::Human => account.bot == Some(false),
            Self::Login(login) => account.login.eq_ignore_ascii_case(login),
            Self::Me => me.is_some_and(|me| account.id.as_deref() == Some(me)),
        }
    }
}

fn account(event: &Event, field: Field) -> Option<&Account> {
    match field {
        Field::Actor => event.actor.as_ref(),
        Field::Subject => event.subject.as_ref(),
        Field::Colour | Field::State => None,
    }
}

/// The field's value in its printed form.
fn word(detail: &Detail, field: Field) -> Option<String> {
    let value = match (field, detail) {
        (Field::State, Detail::ReviewSubmitted { state, .. }) => serde_json::to_value(state),
        _ => return None,
    };
    value.ok()?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ReviewState;

    fn event(detail: Detail) -> Event {
        Event {
            detail,
            url: "https://github.com/o/r/pull/1".to_owned(),
            at: None,
            actor: None,
            subject: None,
        }
    }

    fn created() -> Event {
        event(Detail::CommentCreated {
            comment_id: "1".to_owned(),
            body: "hi".to_owned(),
        })
    }

    fn resolved() -> Event {
        event(Detail::ThreadResolved {
            thread_id: "PRRT_1".to_owned(),
            reply_to: "10".to_owned(),
        })
    }

    fn rules(given: &[&str]) -> Vec<Rule> {
        given.iter().map(|rule| parse(rule).unwrap()).collect()
    }

    fn woken(on: &[&str], ignore: &[&str], event: &Event) -> bool {
        wakes(&rules(on), &rules(ignore), event, None)
    }

    #[test]
    fn every_reported_kind_is_named_as_printed_and_known_to_rules() {
        let thread_id = || "PRRT_1".to_owned();
        let reply_to = || "10".to_owned();
        for detail in [
            created().detail,
            Detail::CommentEdited {
                comment_id: "1".to_owned(),
                body: String::new(),
                thread_id: None,
                reply_to: None,
            },
            submitted(ReviewState::Approved).detail,
            Detail::ReviewWithdrawn {
                review_id: "500".to_owned(),
            },
            Detail::ThreadOpened {
                thread_id: thread_id(),
                comment_id: "10".to_owned(),
                reply_to: reply_to(),
                body: String::new(),
                path: "src/run.rs".to_owned(),
                line: None,
                review_id: None,
            },
            Detail::ThreadReplied {
                thread_id: thread_id(),
                comment_id: "11".to_owned(),
                reply_to: reply_to(),
                body: String::new(),
                review_id: None,
            },
            resolved().detail,
            Detail::ThreadUnresolved {
                thread_id: thread_id(),
                reply_to: reply_to(),
            },
        ] {
            let printed = serde_json::to_value(&detail).unwrap()["kind"].clone();
            assert_eq!(printed, detail.kind());
            assert!(
                KINDS.iter().any(|(kind, _)| *kind == detail.kind()),
                "{printed}"
            );
        }
    }

    #[test]
    fn every_event_wakes_without_rules() {
        assert!(woken(&[], &[], &created()));
    }

    #[test]
    fn kind_patterns_match_exactly_or_by_whole_noun() {
        assert!(woken(&["comment.created"], &[], &created()));
        assert!(woken(&["comment.*"], &[], &created()));
        assert!(!woken(&["comment.edited"], &[], &created()));
        assert!(!woken(&["thread.*"], &[], &created()));
        assert!(woken(&["thread.*", "comment.created"], &[], &resolved()));
    }

    fn account(login: &str, bot: Option<bool>) -> Account {
        Account {
            bot,
            id: None,
            login: login.to_owned(),
        }
    }

    fn by(actor: Account, event: Event) -> Event {
        Event {
            actor: Some(actor),
            ..event
        }
    }

    fn submitted(state: ReviewState) -> Event {
        event(Detail::ReviewSubmitted {
            review_id: "500".to_owned(),
            state,
            body: String::new(),
        })
    }

    #[test]
    fn logins_match_case_insensitively_with_commas_as_or() {
        let gitar = by(account("gitar-bot[bot]", Some(true)), created());

        assert!(woken(&["actor=Gitar-Bot[bot]"], &[], &gitar));
        assert!(woken(&["actor=octocat,gitar-bot[bot]"], &[], &gitar));
        assert!(!woken(&["actor=gitar-bot"], &[], &gitar));
    }

    #[test]
    fn bot_and_human_follow_the_forges_flag_and_an_unknown_kind_matches_neither() {
        let bot = by(account("gitar-bot[bot]", Some(true)), created());
        let human = by(account("octocat", Some(false)), created());
        let unknown = by(account("ghost", None), created());

        assert!(woken(&["actor=@bot"], &[], &bot));
        assert!(!woken(&["actor=@bot"], &[], &human));
        assert!(woken(&["actor=@human"], &[], &human));
        assert!(!woken(&["actor=@human"], &[], &bot));
        assert!(!woken(&["actor=@bot,@human"], &[], &unknown));
    }

    #[test]
    fn terms_are_anded_and_a_missing_field_never_matches() {
        let approved = by(
            account("octocat", Some(false)),
            submitted(ReviewState::Approved),
        );

        assert!(woken(
            &["review.submitted state=approved actor=@human"],
            &[],
            &approved
        ));
        assert!(!woken(&["state=approved actor=@bot"], &[], &approved));
        assert!(woken(
            &["review.* state=changes_requested,approved"],
            &[],
            &approved
        ));
        assert!(!woken(&["state=commented"], &[], &approved));
        assert!(!woken(&["actor=@human"], &[], &resolved()));
        assert!(!woken(&["subject=@human"], &[], &approved));
        assert!(!woken(&["colour=red"], &[], &approved));
    }

    #[test]
    fn subject_matches_the_withdrawn_reviewer() {
        let withdrawn = Event {
            subject: Some(account("octocat", Some(false))),
            ..by(
                account("hubot", Some(true)),
                event(Detail::ReviewWithdrawn {
                    review_id: "500".to_owned(),
                }),
            )
        };

        assert!(woken(&["subject=octocat actor=@bot"], &[], &withdrawn));
        assert!(!woken(&["subject=hubot"], &[], &withdrawn));
    }

    #[test]
    fn me_matches_the_viewers_account_id_not_its_login() {
        let mine = by(
            Account {
                id: Some("U_1".to_owned()),
                ..account("octocat", Some(false))
            },
            created(),
        );
        let me = rules(&["actor=@me"]);

        assert!(wakes(&me, &[], &mine, Some("U_1")));
        assert!(!wakes(&me, &[], &mine, Some("U_2")));
        assert!(!wakes(&me, &[], &mine, None));
        assert!(!wakes(
            &me,
            &[],
            &by(account("octocat", Some(false)), created()),
            Some("U_1")
        ));
    }

    #[test]
    fn only_rules_naming_me_need_the_viewer() {
        assert!(!names_me(&rules(&["actor=@bot,octocat", "colour=red"])));
        assert!(names_me(&rules(&["colour=red", "subject=octocat,@me"])));
    }

    #[test]
    fn accepts_the_specs_examples() {
        for given in [
            "ci.settled colour=red,amber",
            "thread.* actor=@human",
            "review.submitted",
            "review.submitted state=approved actor=gitar-bot[bot]",
            "review.* state=approved",
            "colour=red",
            "subject=@me",
        ] {
            assert!(parse(given).is_ok(), "{given}");
        }
    }

    #[test]
    fn rejects_every_usage_error_naming_the_culprit() {
        for (given, culprit) in [
            ("", "a kind pattern"),
            ("thread.open", "`thread.open`"),
            ("label.*", "`label.*`"),
            ("thread*", "`thread*`"),
            ("*", "`*`"),
            ("thread.* review.submitted", "`review.submitted`"),
            ("author=octocat", "`author`"),
            ("colour=blue", "`blue`"),
            ("state=dismissed", "`dismissed`"),
            ("actor=", "empty"),
            ("actor=octocat,", "empty"),
            ("actor=@admin", "`@admin`"),
            ("actor=a=b", "`a=b`"),
            ("actor=@bot actor=@human", "`actor` appears twice"),
            ("thread.opened colour=red", "`thread.opened`"),
            ("thread.* state=approved", "`thread.*`"),
            ("merge.behind actor=@me", "`merge.behind`"),
        ] {
            let error = parse(given).expect_err(given);
            assert!(error.contains(culprit), "{given}: {error}");
        }
    }

    #[test]
    fn an_ignore_wins_and_drops_only_what_it_matches() {
        assert!(!woken(&["comment.*"], &["comment.created"], &created()));
        assert!(!woken(&[], &["comment.*"], &created()));
        assert!(woken(&[], &["comment.*"], &resolved()));
    }
}
