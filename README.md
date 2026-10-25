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
v=0.1.3 t=aarch64-apple-darwin
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
The CLI creates no identity, grant or trust root.

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

The journal defaults to `~/Library/Application Support/Harklight/Agent` on
macOS, `$XDG_STATE_HOME/harklight-agent` when `XDG_STATE_HOME` is set on Linux,
and `~/.local/state/harklight-agent` otherwise. It stores origin-feed entries,
cursor and operation recovery state under a private directory keyed to the
enrolled device. Keep that state directory if you need to recover an uncertain
submission after a restart.

The journal uses indexed SQLite records. A normal request action reads the
current revision, terminal state, exact referenced event and any unresolved
operation without loading the request's earlier turns; it also avoids rewriting
unrelated history. A prior `origin-feed.json` journal is migrated on first open;
the original file is left unchanged. Feed and operation history has no retention
limit, so keep the state directory on a volume with room for the conversation
history you retain.

## Text conversation commands

Create a request and save the returned session handle:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --title "Choose the next step" \
  --body "Pick one option or write a note."
```

The successful JSON result includes `message_url`, a `harklight://message/…`
link built from the sender ID and message ID. Each ID is percent-encoded as its
own path segment. A committed send returned by `retry` includes the same field.

Every new request carries an `attention-client.actions/1` answer descriptor.
With no `--choice` options, the recipient gets a text Reply action. Repeated
`--choice` options add one value action for each exact choice, followed by text
Reply so the recipient can also answer in their own words:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --title "Choose the next step" \
  --choice "Approve" --choice "Hold"
```

Use `--informational` for a notice that should offer only the exact Seen
action. It cannot be combined with `--choice`:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --title "Update complete" --informational
```

Revisions keep a valid answer descriptor. Replacing choices adds text Reply;
`--clear-choices` leaves a text Reply action instead of removing the descriptor.

The route is required when creating the session handle and is stored with the
CLI version in the server's listening-support record. When sending another
request through an existing handle, omit `--route`; the route stays attached to
that handle. Codex is recorded as unsupported by this build. `other` is
recorded as unsupported unless the same `send --session new` command also
passes `--controller`; see "Other-route listening" below.

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
outcomes; retry that operation with the same UUIDv4 key rather than creating a
second action. A reply, revision, withdrawal or source result acts on the exact
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
  --route other --controller \
  --title "Choose the next step" --body "Pick one option or write a note."
harklight-agent listen --follow --controller --session "$HARKLIGHT_SESSION"
```

`--controller` on `send` requires `--session new --route other` together; it
refuses before any network call otherwise. `--controller` on `listen` requires
`--follow` (not `--next`) and requires that the same handle was minted with
`--route other`; it refuses otherwise. Declaring `--controller` is this
harness's own statement that it runs and supervises that `--follow` process;
the CLI records the arm as supported on that basis alone. It still has no
built-in knowledge of this harness's delivery contract and makes no stronger
claim than for Claude Code or Codex.

## Conversation files

Attach files to a new request, reply or revision with `--file`:

```sh
harklight-agent send --session new --sender-id app:example \
  --route claude-code --title "Review these notes" --file ./notes.pdf
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
separate upload body is not limited by the 2 MiB JSON request bound. Before
submitting the entry, the CLI checks that its final JSON request fits that
existing bound. A file over the published upload cap, an upload failure or an
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

`used` and `unusable` are the agent's own reports, not evidence that a vendor
read or acted on the bytes. Uploaded-file transfer is recorded only after the
CLI verifies the fetched byte count and digest and saves the exact bytes.

## Explicit navigation cleanup

A client may request cleanup only when the person explicitly navigates away
from a message with unsubmitted uploads. The operation is scoped to the exact
sender, message ID and revision and is durably journaled with a UUIDv4 key:

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
