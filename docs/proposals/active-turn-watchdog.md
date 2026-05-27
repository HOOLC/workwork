# Active Turn Watchdog Plan

## Goal

Recover Slack sessions whose broker state still points at an active agent turn even though the agent runtime has stopped producing trace activity. The broker should not leave new Slack messages permanently inflight behind a stale `activeTurnId` after an upstream stream disconnect or hung runtime turn.

## Current state

- Slack sessions persist `agentSessionId`, `activeTurnId`, and `activeTurnStartedAt`.
- The active-turn reconciler periodically reads the runtime snapshot for persisted active turns.
- If the runtime snapshot is `completed`, `interrupted`, or `failed`, the reconciler clears the active turn and marks or resets the inbound batch.
- If the runtime snapshot is `inProgress` or `unknown`, the reconciler always retains the turn.
- If the runtime stream disconnects without a terminal result and the runtime snapshot remains `inProgress`, the broker can keep the turn active indefinitely. Later Slack messages are joined to or queued behind that stale turn and never get a fresh dispatch.
- Manual session reset endpoints already exist, but this failure mode should recover automatically.

## Proposed changes

- Add a configurable stale-active-turn threshold:
  - `SLACK_STALE_ACTIVE_TURN_AFTER_MS`
  - default: `1800000` (30 minutes)
  - values `<= 0` disable the watchdog
- Extend active-turn reconciliation for `inProgress`/`unknown` snapshots:
  - compute the latest broker-observed activity for the active turn from `activeTurnStartedAt` and persisted agent trace events for the same `turnId`;
  - ignore later Slack inbound-message timestamps as activity, because those can be user follow-ups that were blocked behind the stale turn;
  - retain the turn if recent trace activity exists;
  - when the latest activity exceeds the threshold, interrupt the runtime turn when possible, reset that turn's inflight inbound batch to pending, clear the broker `activeTurnId`, and let the existing pending-dispatch recovery path resume the work.
- Keep startup missing-turn recovery behavior unchanged: a missing snapshot is only treated as stale when the startup path explicitly asks for that.
- Document the new environment variable in `.env.example`.

## If we do not change it

A single lost runtime stream can leave a Slack thread wedged until a human notices and manually resets the session. Follow-up messages can make the thread look active while the broker still has no terminal signal to resume dispatch.

## After the change

A long-silent active turn is converted back into pending Slack work automatically. The existing queue drain then starts a new agent turn for the same Slack message batch, so the user sees progress without a manual broker reset.

## Acceptance criteria

- Recent `inProgress` turns are retained.
- Long-silent `inProgress` turns with an inflight batch are interrupted/reset/cleared.
- Runtime interrupt failure does not block broker recovery.
- The threshold is configurable and documented.
- Targeted automated tests and a manual validation command pass.
