---
name: harklight
version: "0.2.16"
description: >-
  Send the person a question, a decision with choices or a notice through
  Harklight, and read their answer, with the harklight-agent command.
  Use this when you need the person's answer or decision and they may be away
  from the terminal.
---

# Harklight

Harklight delivers this agent's questions and notices to the person's Harklight
inbox (iPhone app or web) and returns their answers. The person connects this
computer with `harklight-agent setup` and uses the same command to refresh the
skill. After setup, the `harklight-agent` command needs no connection options.
If you need to check which Server, account or sender IDs this computer can
use, ask the person to run `harklight-agent setup` with no arguments and share
the status it prints. Connecting a disconnected computer requires their
invitation and approval.

Use a sender ID this computer is granted. The setup default grant `app:agent/`
covers any ID that begins with it, such as `app:agent/claude-code` or
`app:agent/codex`. `--display-name` sets the name the person sees.

<!-- Claude Code waiting instructions: pending interactive testing of listen --next. -->
<!-- Codex waiting instructions: pending interactive testing of how to read the answer. -->

# Harklight conversation core

This is the shared core of the self-hosted Harklight agent skill. The package
build appends this text to that skill so the installed skill is self-contained,
and also publishes it as `core.md`; the hosted skill is a separate standalone
guide. Examples describe `harklight-agent` 0.2.2 and the client API v2 server
routes of protocol 0.2. Check the installed version and command help before
using a capability; a published client or this document does not establish a
deployed endpoint or same-session delivery integration.

## Identities and sender grants

An enrolled **device identity** sends under its sender grants and reads the
agent change feed (`/v2/agent`) for messages sent by that enrolled key. It can
also read its person's device inbox through the separate device interface.
Enrollment does not grant ownership or every sender ID. The **sender ID**
identifies the source of a message; a message's identity is its server-issued
`item_id`. Account search can resolve a message ID scoped to a sender.
The Server checks the enrolled device's sender grants for sends.

A separately registered **agent identity** supports search, get and deliberate
answer through separately scoped person routes on the same mutual-TLS device
listener. It reads the person's full view even with empty grants; answering
needs current read and respond grants for the item's sender. It cannot send a
new message. A device enrollment and sender grant do not substitute for agent
registration or those scopes; an agent key does not substitute for
the enrolled device identity used by `harklight-agent`.

Sender patterns use the same rule for sends from an enrolled
device and registered-agent read/respond grants: a pattern ending in `/`
matches every sender ID beginning with that literal prefix; every other
pattern matches exactly. For example,
`app:reports/` covers `app:reports/daily` and `app:reports/team/weekly`, but not
`app:reports` or `app:reports-old/daily`. `app:reports` covers only that exact
sender ID. This is not wildcard or regular-expression matching. Current grants
and revocation still govern each operation. Search's `--sender` filter selects
one exact sender ID, even when a grant covers a prefix.

## Configure the conversation client

Install the separately published client using its
[release instructions](https://github.com/clickety-clacks/harklight-agent#install)
when installation is authorized. It is available for macOS and Linux, arm64
and x86_64, with archive checksums. Installing it neither enrolls a device nor
starts listening. Use the originating device's existing configuration:

| Option | Environment variable |
| --- | --- |
| `--identity-dir` | `HARKLIGHT_IDENTITY_DIR` |
| `--ca-file` | `HARKLIGHT_CA_FILE` |
| `--device-port` | `HARKLIGHT_DEVICE_PORT` |
| `--state-dir` | `HARKLIGHT_AGENT_STATE_DIR` |

The private identity directory contains `server.json`, `enrollment.json`,
`identity.pem` and `identity.key`; the CLI checks the configured CA fingerprint
and authenticates with the device certificate. The device port, which serves
client API v2, is distinct from the enrollment, owner and dashboard ports;
registered agents use the same device port. A new enrollment saves it in
`server.json` as `device_port`. If the identity has no saved device port, set
`--device-port` or `HARKLIGHT_DEVICE_PORT`.

Preserve the private journal and staged files across restarts. Its default is
`~/Library/Application Support/Harklight/Agent` on macOS,
`$XDG_STATE_HOME/harklight-agent` on Linux when that variable is set, otherwise
`~/.local/state/harklight-agent`. It keeps one feed cursor per scope (each
session filter, plus a separate unfiltered scope), entries and operation
recovery state for the enrolled device in SQLite. The first read of a scope
prints the waiting messages and answers it has not delivered yet.
History has no retention limit; keep adequate disk space. Resetting this state
is not a reply-retrieval or uncertain-send recovery procedure.

## Choose the displayed sender name

Use `send --display-name NAME` only with a sender release
that exposes this option and corresponding server and app support. The option
supplies the optional `message.display_name` string on the existing session
send. Agent 0.2.0 includes this option.

With that support, the app displays the supplied name exactly. A notification for
one pending request carries the whole name as its alert title when it fits with
the required metadata; otherwise the notification is still sent with a neutral
sender label. Grouped notifications use a count title and carry as many whole,
deduplicated names as fit, in their existing order, followed by a count of the
remaining names. Names in payloads are never altered, truncated or ellipsized;
other text may be shortened to fit. The phone may clip what it displays. The full
name remains unchanged in the message, feed and history. This adds no name-input
cap or validation, does not reject a name, and does not silently drop a
notification because the name does not fit.
It can be any string, including an empty string, whitespace, newlines or Unicode;
do not normalize it, shorten it or replace it with a registered name. No name
approval, registry, allowlist or naming format is required. `--sender-id` remains
the separate authenticated message identity and its existing grants still apply.
A display name does not grant access or change message routing.
Omit the option to retain the server's existing sender-label behavior.

Save the chosen name with the send operation. Recover an uncertain send with
the same operation ID and saved inputs rather than sending again under another
name. This interface has no `revise --display-name` option. An accepted send is
not evidence that a particular app version or notification displayed the name.

## Send the right message

Choose controls that fit the task. An open question offers text Reply; a
decision offers meaningful choices plus text Reply; an informational notice
offers only Seen. Opening a message is never Seen or an answer. Do not replace
structured choices with numbered options that the person must type.

For a decision:

```sh
harklight-agent send --session new --sender-id app:example --type request \
  --title "Choose the next step" --body "Select an option or write a note." \
  --choice "Proceed" --choice "Defer" --reply
```

Omit `--choice` and keep `--reply` for an open question; `--type request`
needs at least one `--choice` or `--reply`. Use `--type fyi` for a Seen-only
notice; it cannot be combined with `--choice` or `--reply`. Repeated choices
submit their exact label as a JSON string. Every send carries a valid
`attention-client.actions/1` descriptor: text actions return the entered JSON
string; value actions return their exact JSON value. The Seen action is:

```json
{"schema":"attention-client.actions/1","actions":[{"id":"seen","label":"Seen","kind":"value","value":{"acknowledgment":"seen"}}]}
```

A bare text Reply descriptor is:

```json
{"schema":"attention-client.actions/1","actions":[{"id":"reply","label":"Reply","kind":"text"}]}
```

The CLI constructs these descriptors; do not invent a raw-descriptor CLI flag.
An invalid or unfamiliar explicit descriptor has no guaranteed answer control.
Rich `attention-client.ui/1` scenes and other JSON fields require receiving-client
support; an accepted document alone proves no rendering or execution behavior.

Save the returned session handle (a session UUID), item ID, revision and
UUIDv4 operation ID. The committed result carries them as `session_handle`,
`item_id`, `response.message.revision` and `op_id`. A title, shortened display
ID or vendor thread ID is not the session handle. For another request on the
same handle, use `--session` with that handle.

A committed send means server acceptance, not phone display, human attention or
execution of the requested work. `outcome_unknown` is not a refusal. Preserve
the saved operation and exact inputs, then use:

```sh
harklight-agent retry --op-id "$HARKLIGHT_OP_ID"
```

Do not send another message to discover whether an uncertain one committed.
The journal records each operation's exact request before submission, and
`retry` reuses the saved operation key, bytes and precondition. Keep the same
operation key and staged file bytes on recovery. A new intentional action is
different from retrying an unresolved operation. An `outcome_unknown` with
`error:"internal_error"` and `retryable:false` may already have committed; it
stays unresolved under its original operation key, reports the Server's
`request_id`, and is not retried automatically. Replay requires an explicit
`retry`.

## Listen where the originating session supports it

Sending and listening have separate support. Where the originating session
can run a background command, arm one waiter in that same session after a
successful send:

```sh
harklight-agent listen --next --session "$HARKLIGHT_SESSION"
```

The waiter catches up from the saved cursor, then holds an SSE connection to
the agent change feed filtered to that session. It prints each durable change
as a JSON line and exits after durable changes arrive, so the session can
process them and rearm; it does not poll on a timer. A dropped connection,
server shutdown or 60 seconds of read silence reconnects from the last emitted
position, and a restart can replay output already printed. `listen --follow`
is a continuous JSON-lines stream, not the one-waiter background completion
pattern. `read` and `listen` need `--session` or `--all-sessions`; `--all-sessions` follows
every message sent by the enrolled key and takes every session's waiting work.
Feed connections are independent on the server: several readers may follow the
same scope. Readers that share one identity directory share its saved cursor
for that scope.

Revocation and account pause stop the command with a definite diagnostic. A
`500 internal_error` batch or an SSE close with reason `internal_error` stops
it at its last completed position and prints "live updates paused" with the
operation, token and Request-Id on stderr. Start a new invocation to resume
from that position.

Feed output is not proof that a harness received an entry. Real same-session
background-completion delivery must be established by the integration. Do not
claim it from a running process, queued background task or printed line.

The standalone CLI does not provide a managed runtime's wake integration.
An existing runtime may own that integration and deliver entries into its
originating agent session. Follow that runtime's supported send/receive
procedure.

Keep unsupported work with its current task owner, report the actual limitation
and use a supported continuation or a later one-shot read. Do not fabricate a
listener, transfer the sender or introduce a receipt-polling loop. If a background
completion is not delivered, retain the queued work for the next turn of the
same session. When it resumes, catch up with
`read --session "$HARKLIGHT_SESSION"` and rearm if work remains. Loss of a
connection or process does not close a request or conversation.

## Read once without listening

Agent 0.2.0 includes a one-shot read for a session that does not keep a
listener:

```sh
harklight-agent read --session "$HARKLIGHT_SESSION"
```

It fetches the agent change feed once from the saved cursor for that session's
scope, prints each change as a JSON line and ends with
`{"kind":"caught_up","rev":N}`. The server filters the feed to the selected
session; a batch position can pass agent-wide revisions for other sessions,
including in an empty batch. On first contact or cursor expiry it first prints
`{"kind":"baseline",...}`, the waiting messages and answers for that scope and
`{"kind":"baseline_complete","rev":N}`. The CLI commits the scope's cursor after
writing the corresponding output. `--after REV` starts from a caller's own
committed cursor and `--snapshot` takes a fresh baseline instead of the saved
cursor. Serving a person turn or answer records it as Delivered on the server;
that is not a handoff receipt. Read does not apply an answer, open a stream or
promise future background delivery.

Successful output with only `caught_up` means no new change for this scope at
that read. It does not prove closure, absence of an already handled answer or
that no answer will arrive. A failure is not an empty successful read: a failed
read exits nonzero with the diagnostic on stderr and leaves the cursor at the
last completed output. Read again when the originating work resumes; do not
send again or add a polling timer. `result` reports what happened to a
retrieved answer; it does not retrieve one or subscribe to future replies.

## Read a sent message by item ID

Read a sent message by its item ID:

```sh
harklight-agent message-get --item-id "$HARKLIGHT_ITEM"
harklight-agent message-get --item-id "$HARKLIGHT_ITEM" --revision N
```

Omit `--revision` to read the latest version, returned as `{rev,message}`; use
it to select a retained revision, returned as `{rev,revision}`. This read does
not mark the message Seen. `not_found` for a selected revision means the
requested revision is no longer retained; it is gone.

## Handle events and outcomes

Match `item_id`, `session` and `request_revision` against the saved request,
and identify each entry by `entry_id` and its `rev`. Inspect the record `kind`
and, for an entry, its `kind`, `author` and `delivery`; do not classify by body
text. One cursor carries `message`, `entry` and `advisory` records: a `message`
record carries the item, with an answer or close-out in `data.terminal`; an
`entry` record carries person and agent replies, retractions, revisions and
results. An answer or No comment ends the request but leaves chat open.

The server records a person turn or answer as Delivered when it serves it in a
feed response. A newly received person turn or answer needs an explicit handoff
receipt once the originating session has actually received it:

```sh
harklight-agent receipt --item-id "$HARKLIGHT_ITEM" --entry-id "$HARKLIGHT_ENTRY"
harklight-agent receipt --item-id "$HARKLIGHT_ITEM"
```

Omit `--entry-id` for the answer. Receipt and result follow a person's answer,
including No comment. Use `--not-handed REASON` when the handoff
failed. Feed fetch, server Delivered, journal persistence and stdout are not
this receipt. A receipt means handed, not acted. If already delivered and acted,
do not repeat the effect. For a person chat turn, reply to its exact item and
entry; the server derives the current request revision:

```sh
harklight-agent reply --item-id "$HARKLIGHT_ITEM" \
  --in-reply-to "$HARKLIGHT_ENTRY" --body "Here is the requested detail."
```

Discussion is not a terminal answer or an expansion of task authority. A chat
entry can arrive after the request has ended: chat never closes, and either side
may reply in every item state, including after an answer or retraction.
Receiving or replying to that entry does not reopen the request. For a
payload-bearing answered terminal, retain the exact JSON `terminal.answer`,
including the distinction between JSON `null` and an absent answer;
`terminal.outcome` identifies its kind. Apply only the authorized outcome, at
most once, and report it against the item:

```sh
harklight-agent result --item-id "$HARKLIGHT_ITEM" --status applied
```

Use `refused` if the effect cannot be applied, `received` when received but not
confirmed applied. A replay whose `delivery` shows `delivered:true` without
`acted:true` needs an actual effect check: if already done, report applied
without doing it again; if absent, perform the authorized action once; if
uncertain, do not repeat it or claim success. For a re-offered discussion, first
recover any unresolved reply with its original operation ID. If it committed, do
not reply twice. Apply the same effect check to any external work prompted by
discussion.

No comment is a payload-free terminal, identified by
`terminal.outcome.kind:"no_comment"` with no `terminal.answer`. It is not JSON
null, an approval or an instruction to execute the request. Record its handoff
with `receipt` and report `result --status received` for that item to record
receipt without claiming an applied answer; do not invent a payload. Native
clients must submit the displayed request revision for No comment;
missing/stale revisions are refused. The CLI does not update the app.
For cleared, expired or superseded terminals, stop the obsolete request work;
do not report an applied answer. A sender-authored retraction is
retained history, not an actionable prompt or a request to reopen. Closed
messages remain readable; preserve useful drafts and pending answers while
respecting the actual terminal state.

## Revise, retract or replace deliberately

Proactively keep conversations current as work changes, so the person is
not left answering stale questions. Useful ways to do this include revising
the same open request when only its details need correction, superseding it
when a new request replaces it, and retracting an outstanding request that is
obsolete, canceled or no longer needs an answer. Use judgment about when an
update helps and stay within the task's existing authority; these examples
are not an exhaustive checklist or a requirement to mutate every message.

Preserve useful history and already received answers. Retraction withdraws a
request; it does not undo an action already taken. Do not use a replacement to
bypass a terminal refusal. Read the actual operation result and follow the
operation's same-operation recovery rules to resolve uncertainty before
claiming that a request was updated, withdrawn or replaced. An uncertain
outcome is not confirmed cleanup.

An in-place revision keeps the request identity and advances its revision:

```sh
harklight-agent revise --item-id "$HARKLIGHT_ITEM" \
  --base-revision "$HARKLIGHT_REVISION" \
  --title "Updated request" --body "The revised details."
harklight-agent retract --item-id "$HARKLIGHT_ITEM"
```

Use the current base revision and stop acting on a stale draft after revision;
the CLI refuses a stale base revision before sending ("base revision does not
match the current message", exit 2). `--choice` replaces the whole action set,
including a text Reply; pass `--reply` again to keep it. `--clear-choices`
removes the choices; with `--reply` it leaves only the text Reply.
Retraction deliberately withdraws an open request by its item ID; it is not a
person answer and does not erase history. Its optional
`--in-reply-to ENTRY_ID` form also records that person entry as acted on.
Recover an uncertain operation with its original `op_id`; a refusal with
`already_retracted` remains a refusal.

Supersession sends a new message replacing earlier messages from the same sender:

```sh
harklight-agent send --session "$HARKLIGHT_SESSION" --sender-id app:example \
  --type request --reply --title "Replacement plan" \
  --supersedes "$EARLIER_ITEM_1" --supersedes "$EARLIER_ITEM_2"
```

Use earlier item IDs with `--supersedes`. Use `--supersedes-local` with a
committed send operation ID from the same journal and sender. If the journal
cannot resolve it, the CLI stops before posting. Invalid, unknown or closed
item IDs produce `supersedes_warnings` in the Server response. The new message
and valid supersessions still post.

## Files and transfer reports

Repeat `--file PATH` on send, reply or revise to attach files. The CLI stages
the exact bytes, uploads each file separately before the message write and
references the returned upload handles in the write's `file_ids`. The server's
published upload cap applies to each file. `file-upload --file PATH` returns an
upload handle that a later reply or revise can pass with `--file-id`. The
upload body is separate from the 2 MiB JSON entry bound. An exceeded cap,
failed upload, unusable limits or oversized final entry refuses the whole
entry. Keep the draft and files; do not split/truncate
an attachment or silently send only the text. Recovery reuses the same operation
key and staged bytes.

For incoming files, inspect each entry's `files`: each reference carries a
download ID (`file_id`), `name`, `size` and `sha256`. Fetch a file explicitly
with `file-get`; the CLI verifies the bytes before creating the destination.
Transfer and use are reported per entry and file, and the two outcomes are
recorded independently. A transfer report is not proof that the agent read or
used the file. After recording the handoff, report each file in its own
operation:

```sh
harklight-agent file-get --file-id "$HARKLIGHT_FILE" --output notes.pdf
harklight-agent file-use --item-id "$HARKLIGHT_ITEM" \
  --entry-id "$HARKLIGHT_ENTRY" --file-id "$HARKLIGHT_FILE" --transferred --used
harklight-agent file-use --item-id "$HARKLIGHT_ITEM" \
  --entry-id "$HARKLIGHT_ENTRY" --file-id "$HARKLIGHT_FILE_2" --unusable \
  --reason "This session cannot inspect this format."
```

These are the agent's own reports, not independent evidence of vendor action.
Explicit client navigation away from a message with unsubmitted uploads may use
`discard-uploads --file-id HANDLE` for each of its own unattached upload
handles. Journal its operation key and use `retry` after uncertainty. An upload
already attached to a message or entry is refused with `already_closed`.
Preserve text drafts and require reselecting discarded files. Process exit,
disconnect, session end or view rebuild is not that navigation event. The
server keeps unattached uploads for its 30-day retention window, separately.

## Runtime recovery facilities

A runtime that must durably bind its sending-session record before submission
can use `send --prepare --op-id UUID` with the normal send inputs, persist the
returned `prepared` record (operation ID, session handle, local request ID and
immutable metadata including file names, lengths and digests), then call
`send --execute --op-id UUID`. Prepare is local: it stages exact input bytes and
submits neither uploads nor the message. Execute uses only the saved inputs;
recover uncertainty with the same operation. This facility does not create a
listener or a delivery integration.

Use `session end --session HANDLE` only when deliberately reporting that the
originating session is gone; process exit by itself is not a person answer.

## Separately granted agent search and response

Use this only when a registered agent identity and compatible server CLI are
available; the CLI connects to the device port, which serves client API v2. A
person owner registers the public key created by
`agent keygen --directory "$AGENT_DIR"` using `owner agent-register`, then sets
read/respond patterns with `owner agent-grants-replace`. Those are separately
authorized provisioning operations, not repairs to try after a blocked send.

With connection details and the registered identity supplied for this purpose:

```sh
"$HARKLIGHT_SERVER_CLI" agent search --directory "$AGENT_DIR" \
  --connect "$SERVER_ADDRESS" --name "$SERVER_NAME" --port "$DEVICE_PORT" \
  --ca "$CA_PEM" --sender app:example --q "next step"
"$HARKLIGHT_SERVER_CLI" agent get --directory "$AGENT_DIR" \
  --connect "$SERVER_ADDRESS" --name "$SERVER_NAME" --port "$DEVICE_PORT" \
  --ca "$CA_PEM" --item-id "$ITEM_ID" --entries
"$HARKLIGHT_SERVER_CLI" agent get --directory "$AGENT_DIR" \
  --connect "$SERVER_ADDRESS" --name "$SERVER_NAME" --port "$DEVICE_PORT" \
  --ca "$CA_PEM" --sender app:example --message request-1
```

Search (`GET /v2/search`) also accepts `--message`, `--state`, `--limit` and
`--page` with a previous response's `next_page`. Get (`GET /v2/items/{item_id}`)
reads the person's item by `--item-id`, or the item an exact `--sender` and
`--message` name; `--entries` adds its entries page (`--before`, `--limit`).
Each prints the server's JSON body. Reading needs no read grant. To answer an
open message, use the item's `request_revision` from the same get/search item
and a file containing one AnswerBody object: at most one of `choice` (an action
`id`), `comment` or `result`, or `{}` to answer with no comment. Set
`REQUEST_REVISION` from that returned field:

```sh
"$HARKLIGHT_SERVER_CLI" agent answer --directory "$AGENT_DIR" \
  --connect "$SERVER_ADDRESS" --name "$SERVER_NAME" --port "$DEVICE_PORT" \
  --ca "$CA_PEM" --sender app:example --message request-1 \
  --request-revision "$REQUEST_REVISION" --answer-file answer.json
```

`agent respond` is the same command. It is `POST /v2/items/{item_id}/answer`.
The CLI saves the exact body and an `Idempotency-Key` (`--key UUID`, or a
generated one) under `operations/` in the agent directory before anything is
sent. It names its item by `--item-id`, or by an exact `--sender` and
`--message`. The CLI shows the item, revision, key and exact body and asks for
`ANSWER` before submission.
Answering requires current read and respond grants for the item's sender.
Rerunning the same command resends the same body with the same key; the server
replays a committed answer (`"replayed": true`) instead of writing a second one.
Nothing is retried automatically, including after a non-retryable
`internal_error`. A different body for the same item and revision is refused
locally (`operation_conflict`). A changed answer is a new deliberate decision
and may lose to an already committed terminal response. This operation records a response; it
neither executes the source action nor proves phone display. It is not the
originating device's request/listen/reply procedure.
