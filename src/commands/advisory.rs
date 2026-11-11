use super::*;
use crate::journal::AdvisoryEntry;

pub fn ack_advisory(
    context: &CommandContext,
    session: Uuid,
    advisory_id: Uuid,
) -> Result<(), CommandError> {
    if session.get_version() != Some(Version::Random)
        || advisory_id.get_version() != Some(Version::Random)
    {
        return Err(CommandError::new(
            "session and advisory identifiers must be UUIDv4",
            EXIT_REFUSED,
        ));
    }

    let mut store = context.open_store(session)?;
    ack_advisory_with(&mut store, session, advisory_id, |request| {
        context.api.post(request)
    })
}

fn ack_advisory_with(
    store: &mut JournalStore,
    session: Uuid,
    advisory_id: Uuid,
    post: impl FnOnce(&Value) -> Result<Value, ApiError>,
) -> Result<(), CommandError> {
    let entry = store
        .advisory_by_id(session, advisory_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| {
            CommandError::new(
                "advisory is not in this session's durable journal; receive it before acknowledging",
                EXIT_REFUSED,
            )
        })?;

    if entry.delivered {
        return emit(&json!({
            "kind":"advisory_acknowledged",
            "session_handle":session,
            "advisory_id":advisory_id,
            "delivered":true,
            "already_delivered":true
        }));
    }

    if entry.emitted_at_ms.is_none() {
        return Err(CommandError::new(
            "advisory has not been handed off by this session listener",
            EXIT_REFUSED,
        ));
    }

    let request = json!({
        "v":1,
        "type":"advisory_ack",
        "session_handle":session.to_string(),
        "advisory_id":advisory_id.to_string()
    });
    let response = post(&request).map_err(|error| api_error(error, EXIT_REFUSED))?;
    if response["type"] == "error" {
        return Err(CommandError::new(
            format!(
                "origin refused advisory acknowledgment: {}",
                response["reason"].as_str().unwrap_or("unspecified")
            ),
            EXIT_REFUSED,
        ));
    }
    if response["type"] != "ok"
        || response["session_handle"].as_str() != Some(session.to_string().as_str())
        || response["advisory_id"].as_str() != Some(advisory_id.to_string().as_str())
        || response["delivered"] != true
        || !matches!(response["already_delivered"], Value::Bool(_))
    {
        return Err(CommandError::new(
            "origin returned an incomplete or mismatched advisory acknowledgment",
            EXIT_REFUSED,
        ));
    }

    store
        .mark_advisory_delivered(session, advisory_id)
        .map_err(|error| journal_error(error, 1))?;
    emit(&json!({
        "kind":"advisory_acknowledged",
        "session_handle":session,
        "advisory_id":advisory_id,
        "delivered":true,
        "already_delivered":response["already_delivered"]
    }))
}

pub(super) fn fetch_advisories(
    context: &CommandContext,
    store: &mut JournalStore,
    session: Uuid,
) -> Result<bool, CommandError> {
    fetch_advisories_with(store, session, |request| context.api.post(request))
}

fn fetch_advisories_with(
    store: &mut JournalStore,
    session: Uuid,
    mut post: impl FnMut(&Value) -> Result<Value, ApiError>,
) -> Result<bool, CommandError> {
    let since = store
        .advisory_cursor(session)
        .map_err(|error| journal_error(error, 1))?;
    let request = json!({
        "v":1,
        "type":"advisory_feed",
        "session_handle":session.to_string(),
        "since":since
    });
    let response = match post(&request) {
        Ok(response) => response,
        Err(error) if advisory_feed_unsupported(&error) => return Ok(false),
        Err(error) => return Err(api_error(error, 1)),
    };

    if response["type"] == "error" {
        let reason = response["reason"].as_str().unwrap_or("unspecified");
        if advisory_reason_unsupported(reason) {
            return Ok(false);
        }
        return Err(CommandError::new(
            format!("origin refused advisory feed: {reason}"),
            EXIT_REFUSED,
        ));
    }
    if response["type"] != "ok" || response["advisory_v1"] != true {
        return Ok(false);
    }
    if response["session_handle"].as_str() != Some(session.to_string().as_str()) {
        return Err(CommandError::new(
            "origin advisory feed named a different session",
            EXIT_REFUSED,
        ));
    }

    let high_water = response["cursor"]
        .as_u64()
        .ok_or_else(|| CommandError::new("advisory feed has no integer cursor", 1))?;
    if high_water < since {
        return Err(CommandError::new(
            format!("advisory cursor regressed from {since} to {high_water}"),
            1,
        ));
    }
    let entries = response["entries"]
        .as_array()
        .ok_or_else(|| CommandError::new("advisory feed has no entries array", 1))?;
    if usize::try_from(high_water - since).ok() != Some(entries.len()) {
        return Err(CommandError::new(
            "advisory feed omitted or added entries within its cursor range",
            1,
        ));
    }

    let mut journal_entries = Vec::with_capacity(entries.len());
    let mut expected = since.checked_add(1);
    for raw in entries {
        let sequence = raw["advisory_seq"]
            .as_u64()
            .ok_or_else(|| CommandError::new("advisory entry has no integer sequence", 1))?;
        if expected != Some(sequence) {
            return Err(CommandError::new(
                "advisory feed sequence is not contiguous",
                1,
            ));
        }
        expected = sequence.checked_add(1);
        journal_entries.push(parse_advisory(raw, session, sequence)?);
    }
    if entries.is_empty() && high_water != since
        || !entries.is_empty() && expected.and_then(|next| next.checked_sub(1)) != Some(high_water)
    {
        return Err(CommandError::new(
            "advisory feed cursor does not match its entries",
            1,
        ));
    }

    store
        .record_advisory_batch(session, since, high_water, &journal_entries)
        .map_err(|error| journal_error(error, 1))?;
    Ok(true)
}

fn advisory_feed_unsupported(error: &ApiError) -> bool {
    match error {
        ApiError::Http { status: 404, .. } => true,
        ApiError::Refused(reason) => advisory_reason_unsupported(reason),
        _ => false,
    }
}

fn advisory_reason_unsupported(reason: &str) -> bool {
    matches!(reason, "unknown_type" | "not_supported" | "unsupported")
}

fn parse_advisory(
    raw: &Value,
    session: Uuid,
    expected_sequence: u64,
) -> Result<AdvisoryEntry, CommandError> {
    if raw["kind"] != "advisory"
        || raw["advisory_type"] != "unseen_interval_elapsed"
        || raw["author"] != "server"
    {
        return Err(CommandError::new(
            "origin returned an unsupported advisory kind",
            EXIT_REFUSED,
        ));
    }
    let session_handle = raw["session_handle"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|value| value.get_version() == Some(Version::Random))
        .ok_or_else(|| CommandError::new("advisory has an invalid session handle", 1))?;
    if session_handle != session {
        return Err(CommandError::new(
            "origin advisory belongs to a different session",
            EXIT_REFUSED,
        ));
    }
    let advisory_id = raw["advisory_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .filter(|value| value.get_version() == Some(Version::Random))
        .ok_or_else(|| CommandError::new("advisory has an invalid id", 1))?;
    let sequence = raw["advisory_seq"]
        .as_u64()
        .filter(|sequence| *sequence > 0)
        .ok_or_else(|| CommandError::new("advisory has an invalid sequence", 1))?;
    if sequence != expected_sequence {
        return Err(CommandError::new(
            "advisory sequence changed during validation",
            EXIT_REFUSED,
        ));
    }
    for field in ["sender_id", "message_id", "level_name", "text"] {
        if raw[field].as_str().is_none() {
            return Err(CommandError::new(
                format!("advisory has no string {field}"),
                1,
            ));
        }
    }
    for field in [
        "request_revision",
        "accepted_at_ms",
        "due_at_ms",
        "evaluated_at_ms",
        "bounce_after_days",
        "origin_cursor_at_decision",
    ] {
        if raw[field].as_u64().is_none() {
            return Err(CommandError::new(
                format!("advisory has no integer {field}"),
                1,
            ));
        }
    }
    if raw["request_revision"].as_u64() == Some(0) {
        return Err(CommandError::new(
            "advisory has an invalid request revision",
            1,
        ));
    }
    let delivered = raw["delivered"]
        .as_bool()
        .ok_or_else(|| CommandError::new("advisory has no delivered projection", 1))?;
    let origin_cursor = raw["origin_cursor_at_decision"]
        .as_u64()
        .expect("validated above");
    let mut immutable = raw.clone();
    if let Some(fields) = immutable.as_object_mut() {
        fields.remove("delivered");
    }
    Ok(AdvisoryEntry::fetched(
        session_handle,
        sequence,
        advisory_id,
        origin_cursor,
        immutable,
        delivered,
    ))
}

pub(super) fn emit_pending_advisories(
    store: &mut JournalStore,
    session: Uuid,
    follow: bool,
    ordinary_cursor: u64,
    offered: &mut BTreeSet<Uuid>,
) -> Result<bool, CommandError> {
    emit_pending_advisories_with(store, session, follow, ordinary_cursor, offered, emit)
}

fn emit_pending_advisories_with(
    store: &mut JournalStore,
    session: Uuid,
    follow: bool,
    ordinary_cursor: u64,
    offered: &mut BTreeSet<Uuid>,
    mut emit_line: impl FnMut(&Value) -> Result<(), CommandError>,
) -> Result<bool, CommandError> {
    let pending = store
        .pending_advisories_for(session)
        .map_err(|error| journal_error(error, 1))?;
    let mut emitted = false;
    for entry in pending {
        if entry.origin_cursor_at_decision > ordinary_cursor {
            break;
        }
        if offered.contains(&entry.advisory_id) {
            continue;
        }
        let mut line = entry.raw.clone();
        line["delivered"] = Value::Bool(false);
        emit_line(&line)?;
        store
            .mark_advisory_emitted(session, entry.advisory_id)
            .map_err(|error| journal_error(error, 1))?;
        offered.insert(entry.advisory_id);
        emitted = true;
        if !follow {
            break;
        }
    }
    Ok(!follow && emitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, fs};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harklight-advisory-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store(root: &Scratch, session: Uuid) -> JournalStore {
        JournalStore::open(&root.0, "advisory-test-device", session).unwrap()
    }

    fn advisory_response(session: Uuid, id: Uuid) -> Value {
        json!({
            "v":1,
            "type":"ok",
            "advisory_v1":true,
            "session_handle":session.to_string(),
            "cursor":1,
            "entries":[{
                "kind":"advisory",
                "advisory_type":"unseen_interval_elapsed",
                "author":"server",
                "advisory_id":id.to_string(),
                "advisory_seq":1,
                "session_handle":session.to_string(),
                "sender_id":"app:test",
                "message_id":"message-1",
                "request_revision":1,
                "accepted_at_ms":100,
                "due_at_ms":200,
                "evaluated_at_ms":300,
                "level_name":"Normal",
                "bounce_after_days":1,
                "origin_cursor_at_decision":0,
                "text":"No record shows that you opened this message. Evaluation was at 300.",
                "delivered":false
            }]
        })
    }

    #[test]
    fn advisory_fetch_journals_before_offer_and_keeps_cursor_separate() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        let mut journal = store(&root, session);
        let calls = RefCell::new(Vec::new());
        let response = advisory_response(session, id);

        assert!(
            fetch_advisories_with(&mut journal, session, |request| {
                calls.borrow_mut().push(request.clone());
                Ok(response.clone())
            })
            .unwrap()
        );

        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0]["type"], "advisory_feed");
        assert_eq!(calls.borrow()[0]["since"], 0);
        assert_eq!(journal.advisory_cursor(session).unwrap(), 1);
        assert_eq!(journal.cursor().unwrap(), 0);
        assert_eq!(journal.pending_advisories_for(session).unwrap().len(), 1);

        drop(journal);
        let mut journal = store(&root, session);
        assert_eq!(journal.pending_advisories_for(session).unwrap().len(), 1);
        let mut offered = BTreeSet::new();
        let mut output = Vec::new();
        let emitted =
            emit_pending_advisories_with(&mut journal, session, false, 0, &mut offered, |line| {
                output.push(line.clone());
                Ok(())
            })
            .unwrap();
        assert!(emitted);
        assert_eq!(output[0]["kind"], "advisory");
        assert_eq!(output[0]["advisory_id"], id.to_string());
        assert!(
            journal
                .advisory_by_id(session, id)
                .unwrap()
                .unwrap()
                .emitted_at_ms
                .is_some()
        );
        assert!(journal.pending_advisories_for(session).unwrap().len() == 1);
    }

    #[test]
    fn advisory_waits_for_its_recorded_ordinary_prefix_before_handoff() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        let later_id = Uuid::new_v4();
        let mut journal = store(&root, session);
        let mut entry =
            parse_advisory(&advisory_response(session, id)["entries"][0], session, 1).unwrap();
        entry.origin_cursor_at_decision = 2;
        entry.raw["origin_cursor_at_decision"] = json!(2);
        let mut later = parse_advisory(
            &advisory_response(session, later_id)["entries"][0],
            session,
            1,
        )
        .unwrap();
        later.advisory_seq = 2;
        later.raw["advisory_seq"] = json!(2);
        journal
            .record_advisory_batch(session, 0, 2, &[entry, later])
            .unwrap();

        let mut offered = BTreeSet::new();
        let mut output = Vec::new();
        assert!(
            !emit_pending_advisories_with(&mut journal, session, true, 1, &mut offered, |line| {
                output.push(line.clone());
                Ok(())
            },)
            .unwrap()
        );
        assert!(output.is_empty());

        assert!(
            !emit_pending_advisories_with(&mut journal, session, true, 2, &mut offered, |line| {
                output.push(line.clone());
                Ok(())
            },)
            .unwrap()
        );
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["advisory_id"], id.to_string());
        assert_eq!(output[1]["advisory_id"], later_id.to_string());
    }

    #[test]
    fn unsupported_advisory_fetch_leaves_ordinary_feed_available() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut journal = store(&root, session);
        assert!(
            !fetch_advisories_with(&mut journal, session, |_| {
                Err(ApiError::Refused("unknown_type".into()))
            })
            .unwrap()
        );
        assert_eq!(journal.advisory_cursor(session).unwrap(), 0);
        assert_eq!(journal.cursor().unwrap(), 0);
    }

    #[test]
    fn advisory_ack_is_explicit_and_requires_a_journaled_original_session_entry() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        let mut journal = store(&root, session);
        let not_called = RefCell::new(false);
        assert!(
            ack_advisory_with(&mut journal, session, id, |_| {
                *not_called.borrow_mut() = true;
                Ok(Value::Null)
            })
            .is_err()
        );
        assert!(!*not_called.borrow());

        let response = advisory_response(session, id);
        fetch_advisories_with(&mut journal, session, |_| Ok(response.clone())).unwrap();
        let not_emitted_called = RefCell::new(false);
        assert!(
            ack_advisory_with(&mut journal, session, id, |_| {
                *not_emitted_called.borrow_mut() = true;
                Ok(Value::Null)
            })
            .is_err()
        );
        assert!(!*not_emitted_called.borrow());
        let mut offered = BTreeSet::new();
        emit_pending_advisories_with(&mut journal, session, false, 0, &mut offered, |_| Ok(()))
            .unwrap();
        let request_seen = RefCell::new(None);
        ack_advisory_with(&mut journal, session, id, |request| {
            *request_seen.borrow_mut() = Some(request.clone());
            Ok(json!({
                "v":1,
                "type":"ok",
                "session_handle":session.to_string(),
                "advisory_id":id.to_string(),
                "delivered":true,
                "already_delivered":false
            }))
        })
        .unwrap();
        assert_eq!(
            request_seen.borrow().as_ref().unwrap()["type"],
            "advisory_ack"
        );
        assert_eq!(
            request_seen.borrow().as_ref().unwrap()["advisory_id"],
            id.to_string()
        );
        assert!(journal.pending_advisories_for(session).unwrap().is_empty());

        let duplicate_not_called = RefCell::new(false);
        ack_advisory_with(&mut journal, session, id, |_| {
            *duplicate_not_called.borrow_mut() = true;
            Ok(Value::Null)
        })
        .unwrap();
        assert!(!*duplicate_not_called.borrow());

        // A different local journal can learn that the original session has
        // already acknowledged this entry. Reporting that existing fact must
        // not require a local handoff marker or send another receipt.
        let other_root = Scratch::new();
        let other_session = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let mut other_journal = store(&other_root, other_session);
        let mut already_delivered = advisory_response(other_session, other_id);
        already_delivered["entries"][0]["delivered"] = Value::Bool(true);
        fetch_advisories_with(&mut other_journal, other_session, |_| {
            Ok(already_delivered.clone())
        })
        .unwrap();
        let duplicate_receipt_not_called = RefCell::new(false);
        ack_advisory_with(&mut other_journal, other_session, other_id, |_| {
            *duplicate_receipt_not_called.borrow_mut() = true;
            Ok(Value::Null)
        })
        .unwrap();
        assert!(!*duplicate_receipt_not_called.borrow());
    }
}
