use std::fs::File;

use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::v2::feed::Change;

use super::{JournalError, JournalStore, encode_sequence, open_lock_file};

#[derive(Deserialize, Serialize)]
pub(crate) struct DeliveryAttempt {
    pub rev: u64,
    pub first_page: String,
    pub ack2: bool,
    pub expired: bool,
    #[serde(default)]
    pub renew: bool,
    #[serde(default)]
    pub prior: Vec<DeliveryPages>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct DeliveryPages {
    pub rev: u64,
    pub first_page: String,
    pub ack2: bool,
}

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
        self.record_feed(scope, observed, position, changes, rebuild, None)
    }

    pub(crate) fn record_feed_after_delivery(
        &self,
        scope: &str,
        observed: Option<u64>,
        position: u64,
        changes: &[Change],
        baseline_key: &str,
    ) -> Result<(), JournalError> {
        self.record_feed(
            scope,
            observed,
            position,
            changes,
            false,
            Some(baseline_key),
        )
    }

    fn record_feed(
        &self,
        scope: &str,
        observed: Option<u64>,
        position: u64,
        changes: &[Change],
        rebuild: bool,
        baseline_key: Option<&str>,
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
                if !rebuild && change.rev > position {
                    return Err(JournalError::InvalidCursor);
                }
                store_change(tx, scope, change)?;
                if let Some(key) = baseline_key {
                    let resource = change
                        .resource_key()
                        .map_err(|_| JournalError::InvalidCursor)?;
                    tx.execute(
                        "DELETE FROM v2_feed_resources WHERE scope=?1 AND resource=?2 AND rev<=?3",
                        params![key, resource, encode_sequence(change.rev)],
                    )?;
                }
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

    // Serialize the tokenless probe and page application for a single reader
    // scope without holding the database lock during HTTP or stdout writes.
    pub(crate) fn lock_delivery_baseline(&self, key: &str) -> Result<File, JournalError> {
        let hash = hex::encode(Sha256::digest(key.as_bytes()));
        let lock = open_lock_file(
            &self
                .journal_path
                .with_file_name(format!("baseline-{hash}.lock")),
        )?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    pub(crate) fn delivery_attempt(
        &self,
        key: &str,
    ) -> Result<Option<DeliveryAttempt>, JournalError> {
        self.with_read(|connection| {
            let value: Option<String> = connection
                .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
                    row.get(0)
                })
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).map_err(JournalError::from))
                .transpose()
        })
    }

    pub(crate) fn save_delivery_attempt(
        &self,
        key: &str,
        attempt: &DeliveryAttempt,
    ) -> Result<(), JournalError> {
        self.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, serde_json::to_string(attempt)?],
            )?;
            Ok(())
        })
    }

    pub(crate) fn baseline_change(
        &self,
        key: &str,
        change: &Change,
    ) -> Result<Option<Change>, JournalError> {
        let resource = change
            .resource_key()
            .map_err(|_| JournalError::InvalidCursor)?;
        self.with_read(|connection| {
            let value: Option<String> = connection
                .query_row(
                    "SELECT payload FROM v2_feed_resources WHERE scope=?1 AND resource=?2",
                    params![key, resource],
                    |row| row.get(0),
                )
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).map_err(JournalError::from))
                .transpose()
        })
    }

    pub(crate) fn finish_delivery_baseline(
        &self,
        key: &str,
        scope: &str,
        observed: Option<u64>,
        position: u64,
        changes: &[Change],
    ) -> Result<(), JournalError> {
        self.with_transaction(|tx| {
            if cursor(tx, scope)? == observed {
                for change in changes {
                    store_change(tx, key, change)?;
                }
                tx.execute("DELETE FROM v2_feed_resources WHERE scope=?1", [scope])?;
                tx.execute(
                    "INSERT INTO v2_feed_resources(scope,resource,rev,payload)
                     SELECT ?1,resource,rev,payload FROM v2_feed_resources WHERE scope=?2",
                    params![scope, key],
                )?;
                tx.execute(
                    "INSERT INTO v2_feed_cursors(scope,position) VALUES(?1,?2)
                     ON CONFLICT(scope) DO UPDATE SET position=excluded.position",
                    params![scope, position.to_string()],
                )?;
            }
            tx.execute("DELETE FROM meta WHERE key=?1", [key])?;
            Ok(())
        })
    }
}

fn store_change(connection: &Connection, scope: &str, change: &Change) -> Result<(), JournalError> {
    let resource = change
        .resource_key()
        .map_err(|_| JournalError::InvalidCursor)?;
    connection.execute(
        "INSERT INTO v2_feed_resources(scope,resource,rev,payload) VALUES(?1,?2,?3,?4)
         ON CONFLICT(scope,resource) DO UPDATE SET rev=excluded.rev,payload=excluded.payload
         WHERE excluded.rev >= v2_feed_resources.rev",
        params![
            scope,
            resource,
            encode_sequence(change.rev),
            serde_json::to_string(change)?
        ],
    )?;
    Ok(())
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
