//! Current editable Progress material, not a second history or recovery store.
use super::*;

pub(crate) fn is_progress(message: &Value) -> bool {
    message["message_type"] == "fyi" && message.get("progress").is_some()
}

impl JournalStore {
    /// Existing open/feed/settlement paths call this; there is no background
    /// cleanup. An idle process cannot know about an unobserved remote closure.
    pub(crate) fn maintain_progress_material(&self, session: Uuid) -> Result<(), JournalError> {
        let ids = self.with_read(|db| {
            let mut query = db.prepare("SELECT DISTINCT message_id FROM operations WHERE session_handle=?1 AND message_id IS NOT NULL
                AND json_extract(payload,'$.request.message.message_type')='fyi'
                AND json_type(payload,'$.request.message.progress') IS NOT NULL
                UNION SELECT request_id FROM entries WHERE session_handle=?1 AND request_id IS NOT NULL AND kind='revision' AND author='origin'
                AND json_extract(payload,'$.raw.message.message_type')='fyi'
                AND json_type(payload,'$.raw.message.progress') IS NOT NULL")?;
            Ok(query.query_map([session.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })?;
        for id in ids {
            self.prune_progress_request(session, &id)?;
        }
        Ok(())
    }

    pub(super) fn prune_progress_request(
        &self,
        session: Uuid,
        message_id: &str,
    ) -> Result<(), JournalError> {
        // Keep the existing journal lock through commit and filesystem cleanup.
        let guard = open_lock_file(&self.journal_lock_path)?;
        guard.lock_exclusive()?;
        let mut db = self.open_connection()?;
        let mut operations: Vec<OperationRecord> = {
            let mut q = db.prepare(
                "SELECT payload FROM operations WHERE session_handle=?1 AND message_id=?2",
            )?;
            let rows = q
                .query_map(params![session.to_string(), message_id], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.iter()
                .map(|s| serde_json::from_str(s).map_err(JournalError::from))
                .collect::<Result<_, _>>()?
        };
        let mut entries: Vec<JournalEntry> = {
            let mut q = db.prepare(
                "SELECT payload FROM entries WHERE session_handle=?1 AND request_id=?2
                AND kind IN ('revision','terminal') ORDER BY feed_seq",
            )?;
            let rows = q
                .query_map(params![session.to_string(), message_id], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.iter()
                .map(|s| serde_json::from_str(s).map_err(JournalError::from))
                .collect::<Result<_, _>>()?
        };
        if !operations
            .iter()
            .any(|op| is_progress(&op.request["message"]))
            && !entries
                .iter()
                .any(|entry| entry.author == "origin" && is_progress(&entry.raw["message"]))
        {
            return Ok(());
        }
        let mut current: BTreeMap<String, (u64, Value)> = BTreeMap::new();
        let mut closed = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        for op in &operations {
            let sender = operation_sender(op).to_owned();
            if matches!(op.state, OperationState::Prepared | OperationState::Unknown) {
                if let Some(revision) = op.request["base_revision"]
                    .as_u64()
                    .or_else(|| op.request["request_revision"].as_u64())
                {
                    referenced.insert((sender, revision));
                }
            } else if op.state == OperationState::Committed {
                if op.kind == "withdraw" {
                    closed.insert(sender.clone());
                }
                if let Some(revision) =
                    operation_request_revision(op).and_then(|n| u64::try_from(n).ok())
                {
                    if current.get(&sender).is_none_or(|(n, _)| revision > *n) {
                        current.insert(sender, (revision, op.request["message"].clone()));
                    }
                }
            }
        }
        for entry in &entries {
            let sender = entry.sender_id.clone().unwrap_or_default();
            if entry.kind == "terminal" {
                closed.insert(sender.clone());
            }
            if entry.kind == "revision" && entry.author == "origin" {
                if let Some(revision) = entry.request_revision {
                    if current.get(&sender).is_none_or(|(n, _)| revision >= *n) {
                        current.insert(sender, (revision, entry.raw["message"].clone()));
                    }
                }
            }
        }
        let keep_revision = |sender: &str, revision: u64| {
            referenced.contains(&(sender.to_owned(), revision))
                || (!closed.contains(sender)
                    && current
                        .get(sender)
                        .is_some_and(|(n, message)| *n == revision && is_progress(message)))
        };
        // Protect literal references too, including pending operations outside
        // this request. A successor normally owns an independent staged copy.
        let protected: BTreeSet<PathBuf> = {
            let mut q =
                db.prepare("SELECT payload FROM operations WHERE state IN ('prepared','unknown')")?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut paths = BTreeSet::new();
            for row in rows {
                let op: OperationRecord = serde_json::from_str(&row)?;
                paths.extend(op.uploads.into_iter().filter_map(|u| u.staged_path));
            }
            paths
        };
        let mut obsolete = Vec::new();
        let tx = db.transaction()?;
        for op in &mut operations {
            if !is_progress(&op.request["message"])
                || matches!(op.state, OperationState::Prepared | OperationState::Unknown)
            {
                continue;
            }
            let keep = op.state == OperationState::Committed
                && operation_request_revision(op)
                    .and_then(|n| u64::try_from(n).ok())
                    .is_some_and(|n| keep_revision(operation_sender(op), n));
            if !keep {
                let request = redact_file_payloads(&op.request);
                let prepared = op
                    .prepared_send
                    .as_ref()
                    .map(|p| redact_file_payloads(&p.request));
                // Rewrite only rows that still hold material; released rows
                // stay untouched so a pass costs nothing for old history.
                let changed = request != op.request
                    || prepared.as_ref() != op.prepared_send.as_ref().map(|p| &p.request);
                if changed {
                    op.request = request;
                    redact_prepared_send(op);
                }
                for (index, upload) in op.uploads.iter().enumerate() {
                    if let Some(path) = &upload.staged_path {
                        if !protected.contains(path) {
                            obsolete.push((op.op_id, index, path.clone()));
                        }
                    }
                }
                if changed {
                    write_operation(&tx, op)?;
                }
            }
        }
        for entry in &mut entries {
            if entry.author == "origin"
                && entry.kind == "revision"
                && is_progress(&entry.raw["message"])
                && !entry
                    .request_revision
                    .is_some_and(|n| keep_revision(entry.sender_id.as_deref().unwrap_or(""), n))
            {
                let before = entry.raw.clone();
                strip_file_payloads(&mut entry.raw);
                if entry.raw != before {
                    write_entry(&tx, entry)?;
                }
            }
        }
        tx.commit()?;
        // Path metadata stays durable until deletion succeeds. A crash or
        // filesystem error is retried on the next ordinary journal operation.
        for (op_id, index, path) in obsolete {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => continue,
            }
            let tx = db.transaction()?;
            if let Some(mut op) = read_operation(&tx, op_id)? {
                if op.uploads[index].staged_path.as_ref() == Some(&path) {
                    op.uploads[index].staged_path = None;
                    write_operation(&tx, &op)?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }
}

fn operation_sender(op: &OperationRecord) -> &str {
    op.request["message"]["sender_id"]
        .as_str()
        .or_else(|| op.request["sender_id"].as_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cleanup_restarts_after_filesystem_failure_and_protects_any_pending_path_reference() {
        let root = std::env::temp_dir().join(format!("harklight-retention-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let session = Uuid::new_v4();
        let first = Uuid::new_v4();
        let mut store = JournalStore::open(&root, "device", session).unwrap();
        let path = store
            .upload_staging_directory(session, first)
            .join("0.upload");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [1, 2, 3, 4]).unwrap();
        let hash = hex::encode(Sha256::digest([1, 2, 3, 4]));
        let source = UploadSource {
            name: "input.bin".into(),
            staged_path: Some(path.clone()),
            size_bytes: 4,
            sha256: hash.clone(),
            upload_cap_bytes_per_file: None,
            upload_id: Some(Uuid::new_v4().to_string()),
            upload_key: None,
        };
        let original = json!({"sender":"app:test","session":session,
            "message":{"sender_id":"app:test","message_id":"progress","message_type":"fyi","progress":{}},
            "files":[{"name":"input.bin","upload_id":source.upload_id,"size":4,"sha256":hash}]});
        store
            .prepare_v2_operation(
                first,
                session,
                "send".into(),
                original.clone(),
                vec![source.clone()],
            )
            .unwrap();
        let sent = json!({"message":{"item_id":"progress","sender":"app:test","session":session,"revision":1}});
        store.settle_operation(first, sent.clone(), None).unwrap();
        assert!(path.exists());
        let pending = Uuid::new_v4();
        store
            .prepare_v2_operation(
                pending,
                session,
                "reply".into(),
                json!({"message_id":"different"}),
                vec![source],
            )
            .unwrap();
        store.mark_operation_unknown(pending).unwrap();
        let next = Uuid::new_v4();
        store
            .prepare_v2_operation(
                next,
                session,
                "revise".into(),
                json!({
                    "base_revision":1,
                    "sender_id":"app:test",
                    "message_id":"progress",
                    "message":{
                        "sender_id":"app:test","message_id":"progress","message_type":"fyi","progress":{}
                    },
                    "files":[]
                }),
                Vec::new(),
            )
            .unwrap();
        store
            .settle_operation(next, json!({"type":"ok","revision":2}), None)
            .unwrap();
        assert!(
            path.exists(),
            "an unresolved literal reference protects superseded bytes"
        );
        // Force deletion to fail after the replacement is already durable.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        store
            .reject_operation(pending, json!({"type":"error"}))
            .unwrap();
        assert!(
            store.operation(first).unwrap().unwrap().uploads[0]
                .staged_path
                .is_some()
        );
        drop(store);
        let store = JournalStore::open(&root, "device", session).unwrap();
        assert!(path.is_dir()); // no false claim of successful deletion
        drop(store);
        fs::remove_dir(&path).unwrap();
        fs::write(&path, [1, 2, 3, 4]).unwrap();
        let mut store = JournalStore::open(&root, "device", session).unwrap();
        assert!(!path.exists());
        let old = store.operation(first).unwrap().unwrap();
        assert!(old.uploads[0].staged_path.is_none());
        assert_eq!(old.response, Some(sent));
        // Releasing local source bytes does not change committed replay identity.
        assert!(
            store
                .prepare_v2_operation(first, session, "send".into(), original, Vec::new())
                .is_ok()
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
