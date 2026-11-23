# Harklight Agent

Harklight Agent sends messages, revises Progress and handles conversations
from a host command line. The Agent 0.2 protocol requires a Harklight Server
that serves the v2 Agent API.

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

If a successful Server reply fails to confirm the exact send, the CLI
reports it as unconfirmed with exit 1. The operation and reply remain in the
journal and invoking the same operation again will not submit it again.

Use `retry --op-id UUID` after exit 7. The CLI reuses the saved operation key,
bytes and precondition. Use `journal-id` and pass that ID to
`retry --journal-id ID` when a caller must distinguish a missing intent in
the same intact journal from an unresolved operation after losing state.
`abandon --op-id UUID` releases an unresolved local operation. An operation
that already reached the Server can still have taken effect.

## Messages, replies and Progress

- `messages [--session UUID] [--page TOKEN]` lists retained messages.
- `message-get --item-id ID` reads an authoritative message and its revision
  and files. Add `--revision N` for a historical revision. The
  `--sender-id NAME --message-id ID` selector also resolves local references.
- `send-receipt-get --op-id UUID` reads the exact original committed send
  receipt for the enrolled device. It uses the configured device listener
  and does not open the operation journal or repeat the send.
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
- `retract --item-id ID`, `lifecycle --item-id ID --action ACTION`,
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

`read [--session UUID]` catches up once. `listen [--session UUID] --follow`
continues as changes arrive. `listen --next` exits after durable changes
arrive. Both use the same change feed and accept `--after REV` or
`--snapshot`. By default the CLI uses its local checkpoint for that scope.
Several processes may follow the same scope.

Use `--after REV` with your own committed cursor when another process
consumes stdout. On first contact or cursor expiry, the CLI emits
`baseline`, snapshot rows and `baseline_complete`. Commit the baseline
position only after the completion record. `caught_up` supplies the last
complete batch position, including when the batch is empty. Durable records
carry `kind`, `rev` and `data`. The durable kinds are `message`, `entry`
and `advisory`. An answer is in `message.terminal`, including any stored
raw answer. Focus events have no revision.

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
