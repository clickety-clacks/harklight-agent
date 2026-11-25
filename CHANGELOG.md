# Release notes

## Unreleased

- This release speaks only the Harklight 0.2 protocol.
- Resolve sender-qualified message IDs through account search. For Progress on
  an earlier message, pass its Server item ID to `--request`. Local supersession
  resolves recorded send operation IDs from the same journal.
- Use `journal.sqlite3` for new state directories. Existing Agent 0.2 journals
  retain their database and lock paths and reopen with their saved state.
  Remove the retired origin-feed and advisory writers and leave their stored
  history in place.
- Use the saved device port or an explicit `--device-port` or
  `HARKLIGHT_DEVICE_PORT`. The enrollment port is used only for enrollment.

## 0.2.6

- `harklight-agent setup <invitation>` connects an agent in one step: it enrolls
  from the invitation, shows the code to approve and the sender IDs to grant
  (`app:agent/`), waits for approval (and resumes if stopped), checks the grants,
  installs the Harklight skill for Claude Code and Codex where they are present,
  and sends a test question to confirm the answer comes back. With no invitation
  it reports the current state and refreshes the skill. Only this command writes
  the skill files.
- Without `--identity-dir` or `HARKLIGHT_IDENTITY_DIR`, the Agent uses a default
  per-user identity directory. A flag or the environment variable still wins.
- When a send is refused `forbidden`, the error now lists the sender IDs this
  identity may use.
- The README has a quick start and Claude Code and Codex sections, including an
  optional Claude Code notification hook.
- Explain in the README and `send --help` when a message alerts the phone on
  a Server with quiet-by-default alerts.

## 0.2.5

- Add account-wide `search` and senderless message-ID lookup. `messages`
  keeps the own-sent list and adds optional retraction attribution when the
  Server knows it. `message-get` and `message-entries` mark the account-view
  fallback for messages from another sender.
- Retract an open account message by item ID or unique message ID on a
  supporting Server.
- Diagnose the Server's client certificate refusal. `listen --follow`
  repeats the diagnosis while reconnecting and still stops on an explicit
  revoked feed close. Reads and one-shot listens exit with the diagnosis.
  A write interrupted by the late TLS alert keeps its unknown outcome and
  saved operation key.
- `read --pending --prepare` copies the Server's `answers` mark into the
  `baseline_prepared` line when the Server sends it as a string. A Server that
  serves answers entered by a granted agent and default answers sends
  `"answers": "all"`; without the mark, treat the pending read as person answers only.

## 0.2.4

- `read --session UUID --pending --prepare` prepares a pending-only baseline for a
  consumer that keeps its own checkpoint. It prints the pinned revision and a token and
  serves nothing. `read --session UUID --pending --resume TOKEN` serves the waiting Chat
  and answers for that attempt, and can be repeated with the same token after a crash.
- `listen --follow --session UUID --after REV --on-expired exit` reports an expired
  position in a `cursor_expired` line and exits 2 instead of taking a fresh baseline.
- These are for callers that own their checkpoint. Ordinary `read` and `listen` are
  unchanged. One consumer per conversation.

## 0.2.3

- `listen --follow` reconnects after TLS and connection failures, unexpected
  responses and feed `internal_error` failures. It backs off up to 25 seconds
  and resumes from its last completed position. Valid API refusals such as
  `unauthorized`, `forbidden`, `account_paused`, and `not_found` still stop the listener.

## 0.2.2

- `read` and `listen` require exactly one of `--session UUID` or `--all-sessions`,
  including with `--snapshot` and `--after`. Missing or conflicting scopes exit 2
  before loading the identity, opening the journal, or contacting the Server.
  `--all-sessions` takes every session's waiting work and resumes from the
  existing unscoped checkpoint.

## 0.2.1

- Agent-owned default `read` and `listen` recover pending Chat and answers on first contact or cursor expiry on a supporting Server. This includes Chat on answered items and turns retained from 0.1. The local journal retains the attempt across interruption.
- Caller-owned `--snapshot` and `--after` keep Agent 0.2.0 snapshot and resume behavior, including on a supporting Server. Server 0.2.4 and 0.2.5 also keep the earlier snapshot behavior for default reads.
- Known limit: the delivery baseline omits earlier cleared, expired, and superseded items that the 0.2.0 snapshot showed. A valid saved cursor still receives later close-outs as ordinary changes. Use `messages` or `message-get` to inspect terminal state.
- Use `read --ack2` or `listen --ack2` for an integration that issues handoff receipts. The declaration applies only to the receiving scope of the read or listener.
- Keep writes retryable and preserve staged uploads when an HTTP 4xx response does not carry a valid Harklight API error. Reads and listeners point to the configured address or device port when they receive such a response.

## 0.2.0

- Move enrolled Agent operations to the v2 API over mutual TLS. Agent requests and feeds use `/v2/agent`. Shared file operations use `/v2/files`. Use `--device-port` or `HARKLIGHT_DEVICE_PORT` to select the device API port for an existing identity.
- Use `read` for a single catch-up, `listen --follow` for continuous updates, or `listen --next` to wait for durable changes. The old listener lease and exit 6 are removed, along with `attach`, `reconnect`, `cover`, standalone `status`, `ack`, and `ack2`.
- Use Server item IDs for messages and conditional revisions, and entry IDs to correlate replies. Migrated messages can still be resolved by sender and legacy message ID.
- `progress` preserves existing attachments. `revise` keeps them when the file list is omitted and can explicitly replace or clear them.
- `file-upload` returns an upload handle for writes and discard. `file-get` uses the download ID from a message or entry.
- Prepared sends stage files and save an immutable intent locally. `retry` reuses the saved request bytes, operation key, and revision precondition. `abandon` releases an unresolved local operation. The Server may already have applied it.
- Exit 0 means success, 1 a local input, configuration, journal, or read failure, 2 a definite refusal, 5 an unsupported operation, and 7 an unknown write outcome or retryable write.
- A batch `500 internal_error` or an SSE close with reason `internal_error` pauses the feed at its last completed position and reports the Request-Id. Resume with a new invocation. A journaled write that receives this error remains unresolved under its original operation key and reports `retryable: false`. Replay requires an explicit retry.

**Protocol cutover.** Agent 0.2.0 retained unresolved 0.1 journal rows and
pending enrollments. Its v2 feed began with a fresh cursor.

Compatibility: protocol 0.2 (new, supersedes 0.1). Requires a Server that serves the v2 Agent API.

## 0.1.16

- Make existing identity directories and key files private automatically when permissions are too broad. Report an error if the permissions cannot be changed.
- Add `abandon --op-id <KEY>` to unblock a request when an operation’s outcome is unknown. Abandoning stops local retries and does not undo any change already applied by the Server.
- Confirm existing handles with the Server during attach and reconnect. Rebuild local conversation state from the feed when the journal has no record of the original send. Reconnect with no saved attachment still needs the vendor session ID and route.
- Support pending enrollments without deadlines. With a Server release containing #171, restarting a waiting plain enrollment for the same key reuses its approval code and saves the new status secret. Older Server deadlines still parse, and pending output omits deadlines the Server did not supply.
- Fix the file-attachment `send` example to include the required `--type` flag and correct the 0.1.15 release notes.

## 0.1.15

This release intentionally breaks some command-line behaviour. Callers must update.

- `send` now requires `--type`. The `--urgency` and `--informational` aliases are removed; pass `--type` explicitly.
- Removed input refusals: blank stage text, flag-combination refusals and required free-text reasons are no longer rejected by the Agent.
- Enrollment no longer checks label length, and UUIDs are no longer limited by version; any UUID is accepted.
- Removed the client-side 2 MiB request-size pre-check; the Server answers for size.
- Removed the one-time migration of an old `origin-feed.json` journal. An Agent state directory that still holds an old `origin-feed.json` will no longer be migrated.
- Merged the separate ACK2-less attach capability declaration into the single one; `ack2` is still declared only when `--ack2` is given.
- `--controller` is now refused where it would do nothing, restoring some refusals that were removed earlier in this release.
- Removed the origin-wide listener lease. Several origin listeners may now run per enrollment. The per-handle listener lease and its exit code 6 remain.

## 0.1.14

- Progress updates keep ordinary inline content and uploaded files across revisions, and fresh uploads are bound to the new revision.
- Pending or unknown retries replay the original request without uploading files again.
- Files from messages created by older Agent versions cannot be recovered; updates that need them refuse explicitly.
- Reused update keys keep their existing retry and conflict exit codes.

## 0.1.3

- Keep request replies separate from chat in listener output, and continue listening to available chat after an answer or no-comment outcome.

## 0.1.2

- Added `attach` for reconnecting an existing session handle on the same enrolled origin. It updates the vendor session ID and records listener support before opening the stream, while preserving the existing session handle and conversation history.
- Interactive Claude Code listening remains supported with Claude Code 2.1.234 or later and background tasks enabled. A program-owned `other` route requires its controller to consume `listen --follow` and hand events back to the same origin session.
- Codex listening remains unsupported in this version.
