# Release notes

## 0.1.2

- Added `attach` for reconnecting an existing session handle on the same enrolled origin. It updates the vendor session ID and records listener support before opening the stream, while preserving the existing session handle and conversation history.
- Interactive Claude Code listening remains supported with Claude Code 2.1.234 or later and background tasks enabled. A program-owned `other` route requires its controller to consume `listen --follow` and hand events back to the same origin session.
- Codex listening remains unsupported in this version.
