use rusqlite::{Connection, OptionalExtension, params};

use crate::v2::feed::Change;

use super::{JournalError, JournalStore, encode_sequence};

impl JournalStore {
    pub fn v2_feed_cursor(&self, scope: &str) -> Result<Option<u64>, JournalError> {
        self.with_read(|connection| cursor(connection, scope))
    }

    /// Change resources and the default reader's checkpoint share one commit.
    /// A concurrent reader may have already passed a batch this reader saw.
    pub fn record_v2_feed(
        &self,
        scope: &str,
        observed: Option<u64>,
        position: u64,
        changes: &[Change],
        rebuild: bool,
    ) -> Result<(), JournalError> {
        self.with_transaction(|tx| {
            let current = cursor(tx, scope)?;
            if rebuild {
                if current != observed {
                    return Ok(());
                }
                tx.execute("DELETE FROM v2_feed_resources WHERE scope=?1", [scope])?;
            }
            for change in changes {
                let resource = change.resource_key().map_err(|_| JournalError::InvalidCursor)?;
                if !rebuild && change.rev > position {
                    return Err(JournalError::InvalidCursor);
                }
                tx.execute(
                    "INSERT INTO v2_feed_resources(scope,resource,rev,payload) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(scope,resource) DO UPDATE SET rev=excluded.rev,payload=excluded.payload
                     WHERE excluded.rev >= v2_feed_resources.rev",
                    params![scope,resource,encode_sequence(change.rev),serde_json::to_string(change)?],
                )?;
            }
            if rebuild || current.is_none_or(|current| current < position) {
                tx.execute(
                    "INSERT INTO v2_feed_cursors(scope,position) VALUES(?1,?2)
                     ON CONFLICT(scope) DO UPDATE SET position=excluded.position",
                    params![scope, position.to_string()],
                )?;
            }
            Ok(())
        })
    }
}

fn cursor(connection: &Connection, scope: &str) -> Result<Option<u64>, JournalError> {
    let value: Option<String> = connection
        .query_row(
            "SELECT position FROM v2_feed_cursors WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| value.parse().map_err(|_| JournalError::InvalidCursor))
        .transpose()
}
