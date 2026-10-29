# babysit

The language babysit uses for what it watches and what it reports.

## Language

### Watching

**Baseline**:
The snapshot a run starts from: the one the previous run ended on, or the run's own first snapshot
when there is none.
_Avoid_: Cursor, checkpoint, last seen

**Forge**:
A service babysit watches targets on, such as GitHub, GitLab, or Slack.
_Avoid_: Host, provider, platform

**Poll**:
One complete fetch of a target from its forge; a fetch that comes back partial is a failed poll, not
a snapshot.
_Avoid_: Refresh, check, sync

**Snapshot**:
Everything babysit knows about a target at one moment.
_Avoid_: State, status

**Target**:
A single pull request, merge request, issue, or Slack thread being watched.
_Avoid_: Item, watch

### Events

**Actor**:
The forge account that caused an event.
_Avoid_: Author, user, sender

**Bot**:
A forge account the forge itself flags as automated; an ordinary account driven by automation is
not one.
_Avoid_: App, machine user

**Event**:
Something babysit saw happen on a target between two snapshots.
_Avoid_: Change, update, notification

**Event kind**:
The closed category an event belongs to, such as `ci.settled` or `thread.opened`.
_Avoid_: Event type, trigger

**Rule**:
A caller's condition on events, by kind and fields, that decides which events wake babysit.
_Avoid_: Filter, matcher, predicate

**Subject**:
The forge account an event happened to, when that isn't the actor, such as the reviewer whose review
was withdrawn.
_Avoid_: Recipient

**Wake**:
A run ending because at least one event matched its rules.
_Avoid_: Trigger, fire, alert

### Conversation

**Comment**:
A single remark on a target, standing alone or inside a thread.
_Avoid_: Note, message

**Thread**:
A resolvable series of comments on a target. A Slack thread is a target, not a thread: it can't be
resolved, and its parent and replies are each a comment.
_Avoid_: Discussion, conversation

### CI

**Absent CI**:
An open target's head that no check has reported on in the 15 minutes since babysit first saw it
open, reported as `ci.absent`. A push or a reopening restarts the clock, and a check reporting
later still settles it.
_Avoid_: No CI, missing checks

**CI colour**:
The verdict on CI for a target's latest commit: green (every deciding check passed), amber (no
deciding check failed, but CI can't finish until someone acts), or red (a deciding check failed).
_Avoid_: CI status, CI result

**Deciding check**:
A check that must pass for a target's CI to count as passed.
_Avoid_: Blocking check, gate

**Warning**:
A failed check that isn't a deciding check.
_Avoid_: Soft failure, allowed failure
