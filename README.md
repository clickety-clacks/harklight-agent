# Harklight agent conversation CLI

`harklight-agent` sends request-bound Harklight messages and listens to the
origin feed for the same session. It is built and released from this
repository on its own cycle and installs through Homebrew on macOS and Linux.

## Install

```sh
brew install clickety-clacks/tap/harklight-agent
```

Without Homebrew, download the tarball for your platform and the `SHA256SUMS`
file from the [releases page](https://github.com/clickety-clacks/harklight-agent/releases),
verify the checksum, and put the binary on your `PATH`. Substitute the release
version and your target:

| Platform | Target |
| --- | --- |
| macOS, Apple silicon | `aarch64-apple-darwin` |
| macOS, Intel | `x86_64-apple-darwin` |
| Linux, x86_64 | `x86_64-unknown-linux-gnu` |
| Linux, arm64 | `aarch64-unknown-linux-gnu` |

```sh
v=0.1.6 t=aarch64-apple-darwin
base="https://github.com/clickety-clacks/harklight-agent/releases/download/v$v"
curl -fLO "$base/harklight-agent-$v-$t.tar.gz"
curl -fLO "$base/SHA256SUMS"
shasum -a 256 --check --ignore-missing SHA256SUMS   # sha256sum on Linux
tar -xzf "harklight-agent-$v-$t.tar.gz"
install -m 755 "harklight-agent-$v-$t/harklight-agent" ~/.local/bin/
```

Each tarball holds the `harklight-agent` binary, this README and the
third-party license notices. Harklight Server packages from 0.1.10 also carry a
copy of the CLI at `bin/harklight-agent` inside the package; that copy is the
version the package was built with, and it adds no `PATH` entry.

To build from source with Rust 1.89 or later:

```sh
cargo build --locked --release
```

The binary is `target/release/harklight-agent`. The build does not install it.

`harklight-agent --version` prints the release version, which the server
records with each session handle.

## Existing identity and endpoint

The CLI reads the existing device enrollment files `server.json`,
`enrollment.json`, `identity.pem` and `identity.key` from a private identity
directory. It verifies the configured CA fingerprint and authenticates with the
enrolled device certificate. The sender ID still needs its existing grant.
If the identity directory or a file in it is readable by group or other, the CLI
sets the directory to mode 0700 and the key to 0600 and continues; it refuses only
if it cannot change the mode.
The message commands do not create an identity, grant or trust root. Use the
enrollment commands below to create the same identity files the message
commands already read.

Set the identity directory and CA file with either command-line options or
environment variables:

| Option | Environment variable |
| --- | --- |
| `--identity-dir` | `HARKLIGHT_IDENTITY_DIR` |
| `--ca-file` | `HARKLIGHT_CA_FILE` |
| `--origin-port` | `HARKLIGHT_ORIGIN_PORT` |
| `--state-dir` | `HARKLIGHT_AGENT_STATE_DIR` |

The origin HTTPS port is separate from the relay port. Set it in the enrolled
`server.json` as `origin_port` or provide it with `--origin-port` or
`HARKLIGHT_ORIGIN_PORT`; the CLI will not guess it from the relay port. The
server operator must configure and expose the origin endpoint. Agent messages
use this origin session route; the relay endpoint is not a message submission
route.

## Enroll a device

Provide the identity directory and CA file explicitly for every enrollment
command, using the global options or their existing environment variables.
Enrollment also needs the connect address (an IP address or DNS name), TLS name
and enrollment port supplied by the Harklight dashboard or self-host operator.
DNS names stay in `server.json` and are resolved with the platform resolver on
each new connection. The TLS name and CA trust remain separate inputs.

| Command | Command-specific inputs |
| --- | --- |
| `enroll start` | `--connect`, `--name`, `--port`, `--label`; optional `--origin-port` |
| `enroll retry` | `--connect`, `--name`, `--port`, `--label` |
| `enroll status` | `--connect`, `--name`, `--port` |

All three also require `--identity-dir` and `--ca-file` (or their environment
variables above).

```sh
harklight-agent --identity-dir "$HOME/Library/Application Support/Harklight/Identity" \
  --ca-file "$HOME/Library/Application Support/Harklight/ca.pem" \
  enroll start --connect app.harklight.net --name app.harklight.net \
  --port 18444 --label "Work laptop"

harklight-agent --identity-dir "$HOME/Library/Application Support/Harklight/Identity" \
  --ca-file "$HOME/Library/Application Support/Harklight/ca.pem" \
  enroll status --connect app.harklight.net --name app.harklight.net --port 18444
```

`start` creates a private identity directory, generates the device key and
certificate locally, and prints an approval code. Approve that code in the
dashboard and enter the sender ID the device will use there. Then run `status`
once to write `enrollment.json`. Enrollment does not grant a sender ID. Use the
existing `send` command for the first relay/origin proof.

If a reply to `start` is lost, keep the same directory and run `enroll retry`
with its original inputs. A waiting request does not expire: submitting the same
key again returns the same request and approval code with a new status secret,
which `retry` saves in `pending.json` (the previous secret stops working).

Suggested locations (the CA file is beside, not inside, the identity directory):

| Platform | Identity directory | CA file |
| --- | --- | --- |
| macOS | `~/Library/Application Support/Harklight/Identity` | `~/Library/Application Support/Harklight/ca.pem` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/harklight-agent/identity` | `${XDG_DATA_HOME:-~/.local/share}/harklight-agent/ca.pem` |

Use `enroll start` only with a new identity directory; it refuses an existing
one. `enroll retry` reuses the existing key and `enroll status` makes one
status request without polling.

Enrollment failures use these exit codes:

| Server answer or failure | Exit | What to do |
| --- | ---: | --- |
| `409 key_pending` | 2 | The Server already holds a request for this key. Continue its invitation flow if it came from an invitation, or start in a new directory. |
| `409 key_in_use` | 2 | Check `enroll status`, or start in a new directory. |
| `400 invalid_request` | 2 | Check the label and certificate inputs. |
| `404 not_found` | 2 | Run `enroll retry`; if status readback ended, local approved identity data is reported. |
| `503 capacity` or `unavailable` | 2 | The Server did not accept the request; run `enroll retry` later. |
| Other HTTP status or malformed response | 5 | The response is not supported by this Agent. |
| Connection, TLS or timeout failure before a request is sent | 1 | Check the connect address, TLS name and CA file. |
| Request sent without a complete response | 7 | The outcome is unknown. Keep this directory and run `enroll retry`. |

Successful commands print one JSON object to stdout. Approval instructions and
local errors are printed to stderr.

The journal defaults to `~/Library/Application Support/Harklight/Agent` on
macOS, `$XDG_STATE_HOME/harklight-agent` when `XDG_STATE_HOME` is set on Linux,
and `~/.local/state/harklight-agent` otherwise. It stores origin-feed entries,
cursor and operation recovery state under a private directory keyed to the
enrolled device. Keep that state directory if you need to recover an uncertain
submission after a restart.

The journal uses indexed SQLite records. A normal request action reads the
current revision, terminal state, exact referenced event and any unresolved
operation without loading the request's earlier turns; it also avoids rewriting
unrelated history. Feed and operation history has no retention
limit, so keep the state directory on a volume with room for the conversation
history you retain.

## Text conversation commands

Create a request and save the returned session handle:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --type request --reply --title "Choose the next step" \
  --body "Pick one option or write a note."
```

The successful JSON result includes `message_url`, a `harklight://message/…`
link built from the sender ID and message ID. Each ID is percent-encoded as its
own path segment. A committed send returned by `retry` includes the same field.

When a new message replaces earlier messages from the same sender, repeat
`--supersedes` with each earlier request ID:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --type request --reply --title "Updated plan" \
  --supersedes "$EARLIER_ID_1" --supersedes "$EARLIER_ID_2"
```

The server reports an odd or already closed target in `supersedes_warnings`;
valid targets and the new message still post.

For people using Jev, send results may also carry `possible_contradictions`.
This advisory object passes through unchanged on plain `send`, `send --execute`,
`retry`, and committed send records. It never means the message was not delivered.
When matches are listed, talk to their senders to reconcile the competing asks.
An `incomplete` status reports comparisons that could not finish, with `reason`
(`unavailable` or `timeout`) and an `unchecked` count; any matches that did finish
remain listed. A complete check with no matches adds no field.

Every send needs `--type fyi` or `--type request`; a send without one is refused
before any network call (a `--progress` send is always an FYI, so it may leave
`--type` out). `--type fyi` sends only the Seen action. A request needs at least
one `--choice` or `--reply`. Each `--choice` adds one value action, and `--reply`
adds a free-text Reply action:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --type request --title "Choose the next step" \
  --choice "Approve" --choice "Hold" --reply
```

FYIs cannot include choices, Reply, defaults, or choice styles. `--kind` may label a
message as `blocker` or `success`. `--importance` sets the sender's view
(`low`, `normal`, `high`, or `now`); Harklight's ranking decides the priority
the recipient sees. Invalid importance or `--by-when`
values are refused with a correction; `--by-when` takes an RFC 3339 timestamp.

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --type fyi --kind success --importance normal \
  --title "Update complete"
```

For a sender-designed picture choice, put an `attention-client.ui/1` document in
`cover.json`. Name each attached picture in the document using its basename,
then pass that picture with `--file`:

```sh
harklight-agent send --session "$HARKLIGHT_SESSION" --sender-id app:example \
  --type request --title "Choose the cover" --answer-ui cover.json \
  --file cover-a.jpg --file cover-b.jpg
```

`--answer-ui` requires `--type request` and supplies the complete answer
document, so it cannot be combined with `--choice`, `--reply`, defaults, or
choice styles. The CLI preserves the document as JSON and leaves UI/1 grammar
validation to the Server.

Revisions keep the stored type and kind unless `--type` or `--kind` changes
them. Untyped revisions keep today's automatic Reply behavior and preserve a
stored Seen-only notice. Typed requests preserve their existing controls;
`--reply` adds Reply, while replacing choices does not add Reply unless
requested. `--type fyi` revises with only the Seen action and refuses
answer-control flags.

The route is required when creating the session handle and is stored with the
CLI version in the server's listening-support record. When sending another
request through an existing handle, omit `--route`; the route stays attached to
that handle. Codex is recorded as unsupported by this build. `other` is
recorded as unsupported unless the same `send --session new` command also
passes `--controller`; see "Other-route listening" below.

### Progress messages

A Progress message is an FYI that shows a progress bar. You update it in place
under the same message ID. Harklight shows it in its own Progress section, not
as a new item, and leaves it out of ranking and the badge. Send the first report
with `--progress` (a whole percent from 0 to 100), `--progress-stage`, and an
optional `--progress-note`:

```sh
harklight-agent send --session "$HARKLIGHT_SESSION" --sender-id app:example \
  --title "Ship the release" \
  --progress 10 --progress-stage spec_writing --progress-note "Scope agreed"
```

`--progress` sends an FYI, so it can't be combined with `--type request`,
choices, or Reply. Keep the returned `message_id`, then report each change with
`progress`:

```sh
harklight-agent progress --session "$HARKLIGHT_SESSION" --request "$MESSAGE_ID" \
  --percent 40 --stage coding --note "Halfway through the parser"
```

Give only the new percent, the current stage and an optional note. The CLI
builds the bar from the message's current revision in the local journal, so you
never keep or resend the history yourself:

- A higher percent adds width to the stage you name.
- The same percent adds a zero-width visit. Use it for a stage change or a note on its own.
- A lower percent trims the newest widths first and adds a zero-width visit for
  the stage you name. Earlier visits and their notes are kept.

Each visit records its time, the percent you reported and the change from the
previous report. Any label works as a stage. The app gives `spec_writing`,
`coding` and `review` their own colors and shows other labels in a neutral
color. Reaching 100% doesn't close the message; retract it when it should go
away.

An update is an ordinary revise and follows the same rules. For an unknown
outcome (exit 7), run `harklight-agent retry --op-id <KEY>`. It resubmits the
exact same update, and no new update is accepted until it settles. If the
Server refuses the update as stale because the message changed elsewhere, nothing is
applied. Run `harklight-agent read --session <UUID>` to pick up the current
revision, then run the update again. Only a message sent with `--progress` can
be updated this way.

### Prepare a send before executing it

A calling runtime can persist its own authenticated sender record and the exact
prepared operation before it lets the CLI submit the message:

```sh
OP_ID=123e4567-e89b-42d3-a456-426614174000
harklight-agent send --prepare --op-id "$OP_ID" \
  --session new --sender-id app:example --route other --controller \
  --title "Choose the next step" --body "Pick one option."
harklight-agent send --execute --op-id "$OP_ID"
```

`--prepare` stores the operation, exact request and stable file manifest in the
local journal and retains any staged file bytes. It may read the server's
read-only file limits, but it does not upload files or submit the message. The
caller should durably save the returned `op_id`, `request`, and `files` with its
authenticated sending-session record before calling `--execute`. Execute
accepts only the saved operation ID; it does not take replacement send inputs.
It reports the existing `committed`, `refused` or `outcome_unknown` result. For
an uncertain result, use `retry --op-id` with the same operation key.

Only after a definite refusal (exit 2), correct the named input and submit a new
send or revision. A prepared operation stays unchanged; use a new operation for
the correction. For an unknown outcome (exit 7), retry only with
`harklight-agent retry --op-id "$HARKLIGHT_OP_ID"` using the same key and
unchanged inputs; do not submit a changed operation.

### Reattach an existing handle

After a restart on the same host, update an existing handle's vendor session
ID with the same enrolled origin and local journal:

```sh
harklight-agent attach --session "$HARKLIGHT_SESSION" \
  --vendor-session-id "$HARKLIGHT_VENDOR_SESSION_ID" \
  --route claude-code
```

Pass `--vendor-session-id none` when there is no current vendor session ID; the
origin receives JSON `null`. `attach` requires the local journal to contain the
committed send that created the handle. It updates only the handle's vendor ID,
then records the requested route through the existing support operation. It
does not create a request or change the handle or conversation history. Use the
same host, enrollment and journal; this command does not migrate an origin to a
different host or session.

Run a separate `listen` only after `attach` confirms both operations. `listen`
checks the current route support again before opening the stream and leaves
pending turns untouched when the route is unsupported. Codex listening remains
unsupported in this build. If an operation result is unknown, retry the exact
printed operation ID, then repeat `attach` with the same inputs to resume its
next step. Do not change the inputs while an operation is unresolved.

Use the same session handle for every later command. For an interactive Claude
Code session, pass the context and version that the current session reports,
confirm background tasks are enabled, and start one blocking waiter as a
background Bash task in that same session:

```sh
harklight-agent listen --next --session "$HARKLIGHT_SESSION" \
  --session-context interactive \
  --claude-code-version "$(claude --version)" \
  --background-tasks-enabled
```

`listen --follow` continuously writes JSON lines for pending entries;
`listen --next` waits on SSE for one entry, prints one JSON line, and exits. A
change notice contains only a cursor. The CLI fetches authoritative feed entries
after connection, after a newer notice, and after reconnecting; it does not poll
on a timer. It re-offers pending entries from the durable journal after
reconnection.

Each event line includes its server-assigned `channel` at the top level and in
the entry. Every request-side entry, including revisions, results, and all
terminal outcomes, uses `reply`. Only person `discussion` entries and origin
conversation `reply` entries use `chat`. The stream remains one cursor
subscription for both channels. Listening stays open while the status response
reports a positive `open_chats` count, even when `open_requests` is zero after
an answer.

### Read once without listening

A sender whose route does not support listening can still fetch answers and
other pending entries from its own session:

```sh
harklight-agent read --session "$HARKLIGHT_SESSION"
```

`read` makes one request to the existing origin feed using this session's saved
cursor and journal, then prints all currently pending actionable entries as
JSON lines in the same shape as `listen`. It does not arm listening support,
open an SSE stream, or claim that future messages will arrive in the
background. Keep the session's declared route and unsupported listening reason
unchanged. Running `read` again fetches later entries and re-offers any pending
entries that have not been acted on. Reading does not acknowledge delivery;
use `ack` explicitly when a delivery receipt is needed.

Check session availability and pending counts with:

```sh
harklight-agent status --session "$HARKLIGHT_SESSION"
```

To replace an existing origin handle's reply transport, stop and observe your
previous owned worker, then run:

```sh
harklight-agent reconnect --session "$HARKLIGHT_SESSION" \
  --vendor-session-id none --route other --controller
```

Use the existing vendor session ID in place of `none` when applicable. With only
`--session`, reconnect uses the confirmed attachment and route in the same-origin
journal. It refuses a missing attachment or a held listener lock. It does not
create a session, move its origin, resend a message, or replay reply/result
operations. Uncertain attachment/support operations first recover with their
original keys and inputs.

Reconnect makes one transport attempt. After the real SSE handshake and feed
reconciliation, it prints `kind: "reconnected"` with `session_handle` and the
durable origin-wide `feed_cursor`, then streams the usual pending event lines.
An idle conversation prints `kind: "idle"` instead. Pending entries can be
re-offered with the same IDs and delivered/acted flags; consumers must retain
their capture and action receipts. A stream failure ends the worker rather than
starting a hidden retry loop. Standalone `listen` retains its retry behavior.

Failures print `refused`, `outcome_unknown`, or `connection_error` with a stage,
safe reason code/text/source and the saved `op_id` when applicable. An unknown
reason stays explicit; raw response text and credential/path details are not
diagnostics. Attachment metadata and status `listening` do not prove that a
transport is live.

The main command forms are:

```sh
harklight-agent ack --session "$HARKLIGHT_SESSION" --event "$HARKLIGHT_EVENT"
harklight-agent reply --session "$HARKLIGHT_SESSION" --request "$HARKLIGHT_REQUEST" \
  --in-reply-to "$HARKLIGHT_EVENT" --body "The request is complete."
harklight-agent revise --session "$HARKLIGHT_SESSION" --request "$HARKLIGHT_REQUEST" \
  --base-revision 1 --title "Updated request" --body "The revised details."
harklight-agent withdraw --session "$HARKLIGHT_SESSION" --request "$HARKLIGHT_REQUEST"
harklight-agent retract --session "$HARKLIGHT_SESSION" --request "$HARKLIGHT_REQUEST" \
  --op-id "$HARKLIGHT_OP_ID"
harklight-agent result --session "$HARKLIGHT_SESSION" --request "$HARKLIGHT_REQUEST" \
  --in-reply-to "$HARKLIGHT_EVENT" --status applied
harklight-agent read --session "$HARKLIGHT_SESSION"
harklight-agent retry --op-id "$HARKLIGHT_OP_ID"
```

`ack` records delivery only when explicitly run from the originating session.
Feed receipt, stdout output and listener attachment do not prove that handoff.
The CLI persists each operation before submission and reports uncertain
outcomes; retry that operation with the same UUID key rather than creating a
second action. An operation whose outcome stays unknown blocks new operations of
its kind for the same request until it is retried. If you decide not to retry it,
give it up explicitly:

```sh
harklight-agent abandon --op-id "$HARKLIGHT_OP_ID"
```

`abandon` marks that unresolved operation abandoned in the journal, so it stops
blocking and can no longer be retried. The Server may or may not have applied it.
Only an unresolved operation can be abandoned: a committed or refused one is
refused, and abandoning an already abandoned one does nothing. Every other
operation still deduplicates by its key.

A reply, revision, withdrawal or source result acts on the exact
person turn it names. A final answer does not close through local process exit;
only a server terminal entry closes the request.
An origin withdrawal is recorded as a `retracted` terminal entry. Retraction
keeps the request and its history, ends outstanding work, and is not a person
answer. `retract` uses the sender and message identity already recorded in the
selected session journal and submits the existing v1 `withdraw` operation
without an `in_reply_to` reference; it accepts no caller-supplied sender ID.
The `withdraw` command keeps its existing behavior. If a retraction or
answer outcome is uncertain, retry the same journaled operation with its
original UUID; a server refusal with `reason: "retracted"` remains a refusal
and includes the retained retraction state and time.

## No comment result

The server can complete a request with a payload-free `no_comment` terminal;
the listener reports it as the final `NoComment` result on the `reply` channel.
The `no_comment` result leaves the `chat` channel available. A native client must
include the displayed `request_revision` when sending the action (use `1` for
an ordinary request). App build11 omits that field and needs a client update
before this action works; missing or stale revisions do not complete the
request. This CLI does not install or update the native app.

## Other-route listening

A harness that is neither Claude Code nor Codex can declare its own continuous
listener instead of using the Claude Code one-shot waiter pattern:

```sh
harklight-agent send --session new --sender-id app:example \
  --route other --controller --type request --reply \
  --title "Choose the next step" --body "Pick one option or write a note."
harklight-agent listen --follow --controller --session "$HARKLIGHT_SESSION"
```

`--controller` applies only to `--route other` (on `send`, `attach` and `listen`)
and, on `listen`, only with `--follow`; otherwise it is refused before any
network call, because it would do nothing or record support with no supervised
listener behind it. Declaring `--controller` is this harness's own statement that it runs and
supervises that `--follow` process; the CLI records the arm as supported on that basis alone. It still has no
built-in knowledge of this harness's delivery contract and makes no stronger
claim than for Claude Code or Codex.

## Receiving-system ACK2

ACK2 is a receiving-system processing outcome, separate from the agent's Seen
receipt (`ack`) and from any reply or generated text. The published Agent 0.1.6
binary predates this source change and does not include these flags or command.
A controller that sends ACK2 can explicitly advertise that capability when
attaching or listening:

```sh
harklight-agent attach --session "$HARKLIGHT_SESSION" \
  --vendor-session-id none --route other --controller --ack2
harklight-agent listen --follow --controller --ack2 \
  --session "$HARKLIGHT_SESSION"
```

The `--ack2` flag adds `ack2:true` to that command's journaled support
operation. Omitting it declares no ACK2 capability. The receiving
system may submit its processing outcome explicitly:

```sh
harklight-agent ack2 --session "$HARKLIGHT_SESSION" --event "$HARKLIGHT_EVENT" \
  --agent "Assistant" --rerouted
harklight-agent ack2 --session "$HARKLIGHT_SESSION" --event "$HARKLIGHT_EVENT" \
  --not-handed "The receiving system could not hand off this entry."
```

Use `--agent` only when the receiving system has the agent's human-readable
name. `--rerouted` applies only to `handed`; `--not-handed` marks a negative
outcome and may carry a reason. This command submits an ACK2 operation only when the
receiving system explicitly invokes it. It does not mark the agent Seen, infer
delivery from generated output, or automatically retry an uncertain result.
After an unknown outcome, retry only the same journaled operation with
`harklight-agent retry --op-id "$HARKLIGHT_OP_ID"`.

## Conversation files

Attach files to a new request, reply or revision with `--file`:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --type request --reply \
  --title "Review these notes" --file ./notes.pdf
harklight-agent revise --session "$HARKLIGHT_SESSION" \
  --request "$HARKLIGHT_REQUEST" --base-revision 1 \
  --title "Updated request" --file ./diagram.png
harklight-agent reply --session "$HARKLIGHT_SESSION" \
  --request "$HARKLIGHT_REQUEST" --in-reply-to "$HARKLIGHT_EVENT" \
  --body "Here is the annotated result." --file ./annotated.png
```

Before sending files, the CLI asks the origin for its published inline allowance
and upload cap. It sends exact bytes inline while their decoded aggregate fits
the reported per-entry allowance, then uploads any remaining files as exact
bytes and references each server identity, size and SHA-256 in the entry. The
CLI does not check the entry's JSON request size; the Server answers if it
refuses the request. A file over the published upload cap, an upload failure or an
unusable limit response refuses the entry as a whole; it does not split a file,
truncate bytes or send only the text. A retry after an unknown upload response
reuses the same operation key and exact staged bytes.

For a received file, the CLI saves the exact verified bytes under the private
device journal directory in a folder for the session and event before emitting
the entry. The JSON line includes each file's local `path`, `transfer` outcome,
server transfer receipt when known, and a separate `use` outcome that starts as
`unknown`. `ack` reports every file as `transferred` or `not_transferred` with a
reason. The server's receipt confirms transfer only; it does not establish that
the agent used the file.

After acknowledging a received file, report the origin session's use outcome:

```sh
harklight-agent file-use --session "$HARKLIGHT_SESSION" \
  --event "$HARKLIGHT_EVENT" --file "notes.pdf" --used
harklight-agent file-use --session "$HARKLIGHT_SESSION" \
  --event "$HARKLIGHT_EVENT" --file "diagram.png" --unusable \
  --reason "This session cannot inspect this format."
```

`--reason` is optional and valid only with `--unusable`.

`used` and `unusable` are the agent's own reports, not evidence that a vendor
read or acted on the bytes. Uploaded-file transfer is recorded only after the
CLI verifies the fetched byte count and digest and saves the exact bytes.

## Explicit navigation cleanup

A client may request cleanup only when the person explicitly navigates away
from a message with unsubmitted uploads. The operation is scoped to the exact
sender, message ID and revision and is durably journaled with a UUID key:

```sh
harklight-agent discard-uploads --sender-id app:example \
  --message-id "$HARKLIGHT_MESSAGE_ID" --revision 3
```

If the outcome is unknown, run `harklight-agent retry --op-id "$HARKLIGHT_OP_ID"`
with the same key. `state: "committed"` means the scope contains a committed
upload reference; unsubmitted siblings may still have been deleted. The
`discarded` count is the number of unreferenced uploads deleted, and zero does
not prove that any particular file was deleted. The client keeps its text draft
and treats discarded or fenced file references as unready until the person
reselects them. A disconnect, agent exit, session end or view rebuild is not a
navigation event and must not call this command. Separately, the server sweeps
unsubmitted agent uploads after one hour of relevant activity within their
upload group.

## Disposable server fixture

The Harklight server repository carries an integration fixture that drives
this binary against a temporary TLS server. From that repository's root, point
it at a built or installed binary with an absolute path:

```sh
HARKLIGHT_AGENT_BIN="$(command -v harklight-agent)" \
  mix test test/interop/conversation_agent_cli_fixture_test.exs
```

The fixture creates a temporary TLS server, enrolled test identities, and a
temporary journal. It exercises handle creation, unsupported-arm reporting,
same-handle listening, interruption and replay with the same event/request/
revision identity, durable ACK state, re-offer of delivered-but-unacted work,
suppression after a committed reply, and gone status. Person turns are inserted
through the server test API; the fixture does not use the native app or start a
Claude Code or Codex session.

## Support limits

Codex listening is reported as not supported yet; the CLI does not call
`codex queue`. A Codex session may still send a request.
The CLI does not infer a harness from a vendor session ID.

The server publishes the active inline allowance and per-file upload cap.
Self-hosted servers may publish `none`; SaaS deployments must publish their
configured finite cap. The CLI does not invent a SaaS value. File commands
require the origin's file and conversation operations to be available. The
disposable server fixture exercises the Rust process against local server code;
it does not establish delivery into a real vendor session or deployed server.

The Claude Code waiter route requires an interactive local terminal, Claude
Code 2.1.234 or later, and background tasks enabled. The CLI records a missing
prerequisite as unsupported without attaching a stream; after restoring it,
rearm the same session handle so queued turns keep their identities. Claude
Code documents that background commands can continue after a final response,
and documents stops on session exit, a 5 GB output limit, and critical memory
pressure on macOS and Linux. Its background-completion handoff into the same
session has not yet been confirmed in a real interactive session. Until that
is confirmed, report the handoff assumption as unverified; do not replace it
with timer polling or a different agent. See the
[background command reference](https://code.claude.com/docs/en/tools-reference#background-commands)
and [interactive mode reference](https://code.claude.com/docs/en/interactive-mode#background-bash-commands).

## License

MIT. See [LICENSE](LICENSE).

### One listener for an external system

Run `harklight-agent listen --origin --follow` to keep one standing notice
connection for the configured enrollment. Actions such as `ack`, `reply` and
`result` remain separate short requests, using the emitted `session_handle` and
`event_id` with the existing commands. Use the same state directory for the
listener and these commands.

Coverage is explicit. Create a covered conversation with
`send --session new --route other --controller --origin-listener` (plus the usual
message fields), or use `attach --route other --controller --origin-listener`
with the existing handle and vendor-session options. Success requires the server
to confirm coverage. If a send commits but coverage is unsupported, its normal
result remains valid: keep that message and operation identity; do not send it
again. Upgrade the server before enabling coverage.

To move an existing supported route-other handle without a listening gap:

1. Start the origin listener while the existing per-handle listener stays up.
2. Run `cover --session <handle>`. This preserves the handle's current ACK2 and
   advisory capability declarations and takes no per-handle listener lease.
3. After the origin listener has taken over the handle's pending events, stop the
   old per-handle listener. During overlap, deduplicate actions by `event_id`.

Coverage survives server and listener restarts. Restarting a **per-handle**
`listen` on a covered handle replaces its support declaration and **withdraws
origin coverage**. Avoid restarting per-handle listeners for handles that should
remain covered. A declaration without `origin_listener: true` also withdraws
coverage; the origin listener updates its set on the server's coverage notice.

The origin listener stays connected when no requests remain open. On reconnect,
it offers still-pending entries again with their existing `delivered` and `acted`
facts. Deduplicate by `event_id`; a feed cursor records fetch progress, not action
completion. The CLI takes no local lease for the origin listener, so several
origin listeners may run against one enrollment state directory; they share one
journal, which records each event and each operation key once and keeps a
single `delivered` and `acted` fact per event when both listeners see it. Per-handle
`listen` still holds a lease per handle. A replacement connection for the same
enrollment makes the displaced listener exit with listener-active code 6 instead
of reconnecting. An unsupported server produces exit code 5.

The scope is the enrolled identity, including conversations it opened through its
granted senders. Separate systems that need separate aggregation use separate
enrollments; sharing an enrollment shares its scope. There is no sender filter,
all-messages feed, automatic migration, or automatic resend.
