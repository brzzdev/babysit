# babysit

The language babysit uses for what it watches and what it reports.

## Language

### Watching

**Baseline**:
The snapshot a run starts from: the one the previous run ended on, or the run's own first snapshot
when there is none.
_Avoid_: Cursor, checkpoint, last seen

**Forge**:
A code-hosting service babysit watches, such as GitHub or GitLab.
_Avoid_: Host, provider, platform

**Snapshot**:
Everything babysit knows about a target at one moment.
_Avoid_: State, status

**Target**:
A single pull request, merge request or issue being watched.
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
A resolvable series of comments on a target.
_Avoid_: Discussion, conversation

### CI

**CI colour**:
The verdict on CI for a target's latest commit: green (every deciding check passed), amber (no
deciding check failed, but CI can't finish until someone acts) or red (a deciding check failed).
_Avoid_: CI status, CI result

**Deciding check**:
A check that must pass for a target's CI to count as passed.
_Avoid_: Blocking check, gate

**Warning**:
A failed check that isn't a deciding check.
_Avoid_: Soft failure, allowed failure
