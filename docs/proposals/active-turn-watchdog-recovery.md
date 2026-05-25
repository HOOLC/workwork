# Active Turn Watchdog Recovery

## Goal
Keep Slack threads from getting permanently stuck when the agent runtime loses a terminal turn event or leaves a turn in `inProgress` after the agent has stopped producing activity.

## Current state
The broker clears `activeTurnId` when the runtime returns a terminal completion or when `thread/read` reports a terminal turn state. If the runtime keeps reporting an active turn as `inProgress` while no agent-runtime events arrive, newer Slack messages are delivered into that stale turn and the session remains blocked.

## Proposed changes
- Add a configurable active-turn stall timeout based on the latest agent-runtime trace event for the active turn, falling back to `activeTurnStartedAt`.
- During active-turn reconciliation, if `thread/read` still reports `inProgress`/`unknown` but agent activity is older than the timeout, best-effort interrupt the stale turn, reset its inflight messages to `pending`, clear `activeTurnId`, and resume dispatch.
- Add an internal manual repair endpoint for a single session that performs the same reset/resume flow without discarding Slack history or agent session state.
- Cover the flow with unit and e2e tests so follow-up Slack messages are eventually retried in a fresh turn instead of being lost in the stale active turn.

## If we do not change it
A lost terminal event or silent runtime stall can leave the thread stuck indefinitely; follow-up messages will keep joining the stale turn and users will not get the requested PR/final update.

## After the change
A silent active turn is recovered after the configured timeout. The user’s latest Slack messages remain pending and are redelivered to a fresh turn.

## Acceptance criteria
- A test can reproduce an active turn that remains `inProgress` with no agent-runtime activity and verify the broker clears it after the timeout.
- Inflight messages for the stale turn are reset to `pending` and processed by a replacement turn.
- A manual repair API can reset one stale active turn and resume pending work without a full session reset.
- Existing active-turn reconciliation behavior for terminal, missing, and temporarily omitted turns remains intact.
