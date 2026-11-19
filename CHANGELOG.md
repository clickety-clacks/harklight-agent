# Release notes

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
