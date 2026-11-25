# Harklight Agent

Harklight Agent sends messages, revises Progress and handles conversations
from a host command line. The Agent 0.2 protocol requires a Harklight Server
that serves the v2 Agent API.

## Install

Install with Homebrew on macOS or Linux:

```sh
brew install clickety-clacks/tap/harklight-agent
```

Or download the archive for your platform and
`SHA256SUMS` from the
[latest release](https://github.com/clickety-clacks/harklight-agent/releases/latest),
verify the checksum and put the `harklight-agent` binary on your `PATH`.

## Build

```sh
cargo build --locked --release
cargo test --locked
```

## Enrollment

Provide the identity directory, the public Server CA file and the Server
addresses from your operator. Keep the private certificate key in the
identity directory. Enrollment and the mutually authenticated device API
usually use different ports.

```sh
harklight-agent --identity-dir ./identity --ca-file ./server-ca.pem \
  --device-port 18443 enroll start --connect harklight.example \
  --name harklight.example --port 18444 --label "Build agent"
```

Give the approval code to the operator, then use `enroll status` with the
same connection inputs. Use `enroll retry` to recover an interrupted start
with the same generated key. An already enrolled identity can use
`--device-port` or `HARKLIGHT_DEVICE_PORT` to select the v2 device endpoint.

The CLI accepts `HARKLIGHT_IDENTITY_DIR`, `HARKLIGHT_CA_FILE` and
`HARKLIGHT_AGENT_STATE_DIR`. Use the same state directory across invocations
so the journal can recover operations and message references.

## Send and recover writes

```sh
harklight-agent send --session new --sender-id app:build --type fyi \
  --title 'Build complete' --body 'The verification run passed.'
```

Use an existing session UUID to keep the conversation, or omit the session
for a message without a conversation. Use `--file` to attach a file.
`--supersedes` takes a Server item ID. `--supersedes-local` resolves a local
reference for the same sender before delivery.

For two-phase delivery, add `--prepare --op-id UUID`. The CLI stages the
files and writes the immutable intent locally. Run
`send --execute --op-id UUID` to deliver it. The `prepared` and `committed`
JSON records carry the same local request and operation IDs. A committed
send also reports the opaque Server `item_id`.

Use `retry --op-id UUID` after exit 7. The CLI reuses the saved operation key,
bytes and precondition. Use `journal-id` and pass that ID to
`retry --journal-id ID` when a caller must distinguish a missing intent in
the same intact journal from an unresolved operation after losing state.
`abandon --op-id UUID` releases an unresolved local operation. An operation
that already reached the Server can still have taken effect.

## Messages, replies and Progress

- `messages [--session UUID] [--page TOKEN]` lists the current identity's
  retained sent messages with the existing response shape.
- `search [--session UUID] [--sender NAME] [--message-id ID] [--state STATE]
  [--page TOKEN]` lists retained messages across the account, newest first.
  Repeat the filters with the returned `next_page` token. Servers that lack
  account search support refuse with `unsupported` (exit 5).
- `message-get --item-id ID` reads a message and its file references. Add
  `--revision N` for a historical revision. `--message-id ID` resolves a
  unique ID across the account. The existing `--sender-id NAME --message-id
  ID` selector still supports qualified legacy IDs.
- `message-entries --item-id ID` or `--message-id ID` pages through entries.
  Both reads prefer the existing sent-message view. If the Server answers
  `not_found` or `forbidden` for that view, the CLI tries the account view and marks the
  response with `"view":"account"`. The account message uses the person's
  `item` shape, with no origin-only session, legacy ID alias, raw answer
  or advisories. Account entries omit the sender's delivery and file-use
  receipts. Use `file-get` to download a referenced attachment.
- `reply --item-id ID --body TEXT` sends a chat turn. Add
  `--in-reply-to ENTRY` to correlate a person entry. A reply can carry files
  with no text.
- `revise --item-id ID --base-revision N --message-json PATH` submits a full
  revision. Use `--title` and content options for an interactive revision.
  An omitted file list keeps the current attachments. `--clear-files` clears
  the list and `--file-id` or `--file` supplies a replacement set.
- `send --progress PERCENT --progress-stage STAGE` creates a Progress FYI.
  Use `progress --session UUID --request ITEM --percent PERCENT --stage STAGE`
  to revise it conditionally. The CLI reads the current content and revision
  together and keeps its existing attachments. Retry a lost outcome with
  the original operation key.
- `retract --item-id ID` or `retract --message-id ID` retracts an open account
  message. If the message ID matches several items, use the intended item ID.
- `lifecycle --item-id ID --action ACTION`,
  `result --item-id ID --status STATUS` and `session end --session UUID`
  apply their respective state changes.
- `receipt --item-id ID` records answer handoff. Add `--entry-id ENTRY` for
  chat handoff. Use `--not-handed REASON` when handoff failed.
- `file-use --item-id ID --entry-id ENTRY --file-id ID` reports transfer
  or use for one file. Use `--transferred`, `--not-transferred`, `--used` or
  `--unusable`. A negative outcome requires `--reason`.

`file-upload --file PATH` returns an upload handle for writes.
`file-get --file-id ID --output PATH` takes a download ID and verifies the
bytes before creating the destination. `discard-uploads --file-id HANDLE`
discards an unattached upload.

## Change feed

`read --session UUID` catches up once for one session.
`listen --follow --session UUID` continues as changes arrive.
`listen --next --session UUID` exits after durable changes arrive.
`listen --follow` reconnects after connection or TLS failures, unexpected
responses, and feed `internal_error`. It resumes from its last completed
position, waits up to 25 seconds between attempts, and reports failures and
recovery on stderr. Valid API refusals and local configuration errors still
exit. When the Server refuses the client certificate, follow mode repeats
`this identity is not accepted (revoked or never enrolled)` on each failed
attempt. It still exits on an explicit `revoked` feed close. Reads and one-shot
listens exit 1 on the certificate refusal. The late TLS alert does not
establish that a write was unprocessed, so the CLI keeps exit 7 and the
saved operation key for retry.

Every `read` and `listen` invocation requires exactly one of `--session UUID`
or `--all-sessions`, including with `--after REV` or `--snapshot`. Use
`--session UUID` for your own session when agents share an identity.
`--all-sessions` takes every session's waiting work and resumes from the same
checkpoint as an unscoped invocation in earlier versions. By default the CLI
uses its local checkpoint for the chosen scope. Several processes may follow
the same scope.

When another process owns the checkpoint, start with `--snapshot` and
resume with `--after REV` once that process records its applied cursor.
Use the same identity, endpoint, session filter and state directory across
both commands, and give each independent consumer its own state directory.
Both explicit starts keep the Agent 0.2.0 snapshot and resume behavior on
all Servers and leave the CLI's default checkpoint untouched. A supplied
`--after REV` is a feed cursor.

### Caller-owned pending work

For a consumer with its own durable checkpoint, prepare a pending-only
baseline with `read --session S --pending --prepare`. It returns one
`baseline_prepared` JSON line containing `session`, `rev` and `token`
without serving work or opening the Agent journal. Store the token and
revision durably before requesting any pages.

Use `read --session S --pending --resume TOKEN` to serve that attempt.
Output starts with `baseline` and ends with `baseline_complete`. The
rows use the feed's `kind`, `rev` and `data` shape and include retained
pending person Chat and answer rows, including on closed items and
migrated 0.1-era turns. An answer is a message with an answered terminal
state. Apply a still-unapplied row even if it carries `pending:false`.
The marker describes a receipt, action or updated representation.

After interruption, run resume again with the original token and the
same session and `--ack2` setting. Apply by stable entry ID or answer
item ID and commit the pinned revision with the applied effects before
forgetting the token. A rejected token results in `token_invalid` with
the Server reason (exit 2). A Server without the route results in
`unsupported` (exit 5). Both commands use only the pending route.
This feature requires Agent 0.2.4 or later.

Start `listen --follow --session S --after REV --on-expired exit` after
committing the baseline. It prints a `cursor_expired` JSON line with the
last position and exits 2 on a typed HTTP 410 or an SSE expiry notice.
The caller owns the recovery decision and its checkpoint.

Use one consumer per conversation. An entry another reader marked
Delivered before the pin is excluded, and an update above the pinned
revision is not guaranteed. Restoring the caller database to a point
before the token was stored loses access to work already served under
that token. Work that was never served remains pending.

### Agent-owned baselines

On first contact or cursor expiry, the CLI emits
`baseline`, baseline rows and `baseline_complete`. With Server support,
Agent-owned default reads and listeners recover pending Chat and answers,
including Chat on answered items. Use `messages` and `message-entries`
to inspect historical state.
The CLI keeps its active attempt in the journal. Restart with the same
state directory and session scope to recover an interrupted attempt. An
incomplete baseline can replay rows already printed. If an interrupted
attempt's feed position has expired (HTTP 410), the CLI prints a new baseline
that can replay rows even after an earlier `baseline_complete`. Deduplicate by
item ID for answers and entry ID for Chat. A baseline row with `pending: false` updates an
existing turn or a turn with a recorded action or receipt.

Commit the baseline position only after the completion record.
`caught_up` supplies the last complete batch position, including when
the batch is empty. Durable records carry `kind`, `rev` and `data`.
The durable kinds are `message`, `entry` and `advisory`. An answer is in
`message.terminal`, including any stored raw answer. Focus events have
no revision. Server 0.2.4 and 0.2.5 use the earlier snapshot and feed.

Add `--ack2` to a scoped read or listener when the consumer sends handoff
receipts for the turns it receives. On an older Server the feed works as
before and the capability is unknown. An active baseline retains its original
declaration through retries until the checkpoint is covered.

### Hosted upgrade to 0.2.1

With the Server that supports the delivery baseline, keep using
`harklight-agent read --session UUID` for the first or subsequent read and
`harklight-agent listen --follow --session UUID` for a continuous listener.
Run the same command with the same state directory after interruption.
Use the default Agent-owned checkpoint on Hosted to recover pending work
when the checkpoint is missing or expired. Caller-managed `--snapshot`
and `--after` invocations keep the Agent 0.2.0 behavior, including its gap
for waiting Chat on answered items during a historical snapshot. Server
0.2.4 and 0.2.5 retain that earlier behavior for default reads as well.

Use `focus` to read current focus and `show --item-id ID [--chat]` to request
a view on a device. `advisory-ack --item-id ID --advisory-id ID` records
advisory delivery.

## Exit status

| Exit | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Local input, configuration, journal or read failure |
| 2 | Definite refusal |
| 5 | Unsupported operation |
| 7 | Write outcome unknown or retryable |

[Release packaging and fleet delivery](docs/fleet-delivery.md)
