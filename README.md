# Harklight Agent

Harklight is an attention-based UI for messages and requests from your agents.
Use the Harklight Agent CLI to send messages, revise Progress and handle
conversations from the command line. The Agent 0.2 protocol requires a Harklight Server
that serves the v2 Agent API.

## Quick start

Join your Harklight account in the iPhone app first. In **Devices**, create an invitation and
choose **Copy full link**. On the computer that runs Claude Code or Codex:

```sh
brew install clickety-clacks/tap/harklight-agent
harklight-agent setup 'harklight://join#…'
```

In **Devices**, choose **Review exact request**, match the displayed code and enter
`app:agent/` in **Sender IDs** before approving. This prefix lets the tools on this
computer share the enrollment. Answer the test question on your phone to finish.

If you interrupt the approval wait, run `harklight-agent setup` to resume with the same code.
Once connected, the same command shows the Server, account, device and sender grants and
refreshes the installed skill. Run setup yourself when you want to install or refresh the
skill. Setup also accepts an invitation file and an optional
`--label "Name in Devices"`. If you approved without granting any sender IDs, remove that
device in **Devices**, create a fresh invitation and run setup again.

## Claude Code

Setup installs the Harklight skill at `~/.claude/skills/harklight/SKILL.md` when
`~/.claude` exists. In Claude Code, ask “Ask me through Harklight whether to proceed, then
wait for my answer.” The default sender ID for Claude Code is `app:agent/claude-code`.

### Optional waiting notices

To have Claude Code send a Seen-only notice when you appear to be away, add this
[Notification hook](https://code.claude.com/docs/en/hooks#notification) to
`~/.claude/settings.json`. If you already have hooks, add the entry to your existing
`Notification` array. The hook also fires when Claude Code has already asked a Harklight
question and is idle waiting for your answer. Remove the entry to turn off notices.

```json
{
  "hooks": {
    "Notification": [
      {
        "matcher": "idle_prompt|permission_prompt",
        "hooks": [
          {
            "type": "command",
            "command": "harklight-agent send --sender-id app:agent/claude-code --type fyi --title 'Claude Code is waiting' >/dev/null",
            "async": true
          }
        ]
      }
    ]
  }
}
```

## Codex

Setup installs the Harklight skill at `~/.agents/skills/harklight/SKILL.md` when
`~/.codex` or the directory in `CODEX_HOME` exists. In Codex, ask “Ask me through
Harklight whether to proceed, then wait for my answer.” The default sender ID for Codex is
`app:agent/codex`. Codex has no matching event for the waiting-notice recipe.

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

To start enrollment with an invitation file without running the setup checks, use
`harklight-agent enroll start --invitation PATH`, then `harklight-agent enroll status`.
The file may contain the invitation JSON or the full `harklight://join#…` link.

For manual enrollment, provide the public Server CA file and the Server addresses from
your operator. Use `--identity-dir` or `HARKLIGHT_IDENTITY_DIR` to select an identity
directory. The default is `~/Library/Application Support/Harklight/Agent/identity` on
macOS and `$XDG_DATA_HOME/harklight-agent/identity` on Linux (with
`~/.local/share/harklight-agent/identity` as the fallback). The flag takes precedence
over the environment variable. Keep the private certificate key in the
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
`--device-port` or `HARKLIGHT_DEVICE_PORT` to override the saved device port.
Set one of them if the identity has no saved device port.

The CLI accepts `HARKLIGHT_IDENTITY_DIR`, `HARKLIGHT_CA_FILE` and
`HARKLIGHT_AGENT_STATE_DIR`. Use the same state directory across invocations
so the journal can recover operations and message references.

## Send and recover writes

```sh
harklight-agent send --session new --sender-id app:build --type fyi \
  --title 'Build complete' --body 'The verification run passed.'
```

On a Server with quiet-by-default alerts, a request or a message with
`--importance high` or `--importance now` alerts the recipient's phone.
Other messages arrive quietly unless the recipient has chosen to receive all
alerts.

Use an existing session UUID to keep the conversation, or omit the session
for a message without a conversation. Use `--file` to attach a file.
`--supersedes` takes a Server item ID. `--supersedes-local` takes a committed
send operation ID from the same journal and sender.

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

## Shared activity

Run `capabilities` to see the enrolled Server's immutable ID and whether it
supports shared messaging. To send one message to several people, repeat
`--person-id ID` on `send`. If the Server returns `organization_required`,
choose one of the returned organization IDs and submit a new send with
`--organization ID`. The first answer settles a request unless you specify `--decider-id ID`. For choices with stable IDs and separate
labels, pass `--choices-json PATH` with a JSON array of
`{"id":"approve","label":"Ship it"}` objects.

Use `message-get --shared --item-id ID` and
`message-entries --shared --item-id ID` to read a shared conversation.
Shared replies identify the speaker by Server-scoped ID and show a display
name and handle when provided. The message identifies who settled it.
The sender can use `retract --shared --item-id ID` to withdraw the message
or `chat-close --item-id ID` to stop chat while preserving history.

For integrations, prepare with `read --shared --pending --session UUID --prepare`
and resume the pending baseline with `--resume TOKEN`. After accepting the
baseline, use `read --shared --session UUID --after REV --continuation TOKEN`
to catch up or use the same options with `listen --follow` to stay current.
Use the continuation from the prepared baseline alongside its revision, then
advance the revision as you accept changes. Start a fresh baseline if the
Server reports an expired cursor. The shared feed identifies each person's
Seen and each answer. Acknowledge an answer with
`receipt --item-id ID --answer-id ID`, and a reply with
`receipt --shared --item-id ID --entry-id ID`.

### Groups

On a Server that advertises groups, use `send --group-id ID` to reach the
group. A self-hosted Server also accepts `send --group-name NAME`. The
`groups` command lists the groups the authenticated person has joined.
A group's ID is distinct from the item ID returned by a send.

Use `--groups` on `messages`, `message-get`, `message-entries`, `read`,
`listen`, `reply`, `revise`, `receipt`, `chat-close`, and `retract`
for group conversations. Before reading or following group activity, prepare
and resume a fresh pending baseline with `--groups`. Use the same flag
and session with its continuation. A retry uses the scope saved in the
operation journal.

## Messages, replies and Progress

- `messages [--session UUID] [--page TOKEN]` lists the current identity's
  retained sent messages with the existing response shape.
- `search [--session UUID] [--sender NAME] [--message-id ID] [--state STATE]
  [--page TOKEN]` lists retained messages across the account, newest first.
  Repeat the filters with the returned `next_page` token. Servers that lack
  account search support refuse with `unsupported` (exit 5).
- `message-get --item-id ID` reads a message and its file references. Add
  `--revision N` for a historical revision. `--message-id ID` resolves a
  unique ID across the account. Add `--sender-id NAME` to restrict the lookup
  to that sender and read the account view.
- `message-entries --item-id ID` or `--message-id ID` pages through entries.
  Both reads prefer the existing sent-message view. If the Server answers
  `not_found` or `forbidden` for that view, the CLI tries the account view and marks the
  response with `"view":"account"`. The account message uses the person's
  `item` shape and omits the sender-view session, raw answer and advisories.
  Account entries omit the sender's delivery and file-use receipts. Use `file-get` to download a referenced attachment.
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
attempt. `focus --follow` also reports the refusal and retries when
reconnecting. An explicit `revoked` feed close still ends the listener. Reads and one-shot
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
pending person Chat and answer rows, including on closed items.
An answer is a message with an answered terminal state. Apply a still-unapplied row even if it carries `pending:false`.
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
