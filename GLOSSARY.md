# babysit

The language babysit uses for what it watches and what it reports.

## Language

**Target**:
A single pull request, merge request or issue being watched.
_Avoid_: Item, watch

**Snapshot**:
Everything babysit knows about a target at one moment.
_Avoid_: State, status

**Event**:
One thing that happened on a target.
_Avoid_: Change, update, notification

**Event kind**:
The closed category an event belongs to, such as `ci.settled` or `thread.opened`.
_Avoid_: Event type, trigger

**Thread**:
A discussion on a target that can be resolved.
_Avoid_: Discussion, conversation

**Comment**:
A remark on a target that isn't part of a thread.
_Avoid_: Note, message

**Actor**:
The forge account that caused an event.
_Avoid_: Author, user, sender

**Subject**:
The forge account an event happened to, such as the reviewer whose approval was withdrawn.
_Avoid_: Recipient

**Deciding check**:
A check that must pass for a target's CI to count as passed.
_Avoid_: Blocking check, gate

**Warning**:
A failed check that isn't a deciding check.
_Avoid_: Soft failure, allowed failure

**CI colour**:
The verdict on a target's CI: green (every deciding check passed), amber (none failed, but CI can't
finish until someone acts) or red (one failed).
_Avoid_: CI status, CI result
