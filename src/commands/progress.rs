//! Progress messages: an ordinary FYI carrying a top-level `progress` member
//! that the sender revises in place under the same message ID.
//!
//! The caller supplies only the percent, the current stage label and an
//! optional note. The CLI builds the complete ordered segment list from the
//! message's current carried fields, so the caller never maintains history.

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    CommandContext, CommandError, EXIT_REFUSED, OperationTransport, emit_committed,
    ensure_request_open, journal_error, now_ms, request_context, submit_operation_with_transport,
};
use crate::files::{self, PreparedFiles, ServerFileLimits};
use crate::journal::JournalStore;

/// One progress report: the caller's complete input for a send or update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressReport {
    pub percent: u8,
    pub stage: String,
    pub note: Option<String>,
}

impl ProgressReport {
    pub(super) fn validate(&self) -> Result<(), CommandError> {
        if self.percent > 100 {
            return Err(CommandError::new(
                format!(
                    "progress percent must be from 0 to 100; got {}",
                    self.percent
                ),
                EXIT_REFUSED,
            ));
        }
        Ok(())
    }

    pub(super) fn identity(&self) -> Value {
        json!({"percent":self.percent,"stage":self.stage,"note":self.note})
    }
}

/// The `progress` member for a newly created Progress message.
pub(super) fn initial_progress(report: &ProgressReport, at_ms: u64) -> Value {
    compose(0, Vec::new(), report, at_ms)
}

/// The next `progress` member, built from the message's current one.
pub(super) fn next_progress(
    current: &Value,
    report: &ProgressReport,
    at_ms: u64,
) -> Result<Value, CommandError> {
    let (prior_percent, segments) = carried_progress(current)?;
    Ok(compose(prior_percent, segments, report, at_ms))
}

fn compose(
    prior_percent: u64,
    mut segments: Vec<Value>,
    report: &ProgressReport,
    at_ms: u64,
) -> Value {
    let percent = u64::from(report.percent);
    let at = rfc3339_utc(at_ms);
    let width = if percent >= prior_percent {
        percent - prior_percent
    } else {
        // Trim the newest filled widths first; earlier visits keep their
        // width until trimming reaches them. Only widths change.
        let mut remaining = prior_percent - percent;
        for segment in segments.iter_mut().rev() {
            if remaining == 0 {
                break;
            }
            let filled = segment["width"].as_u64().unwrap_or(0);
            let taken = filled.min(remaining);
            segment["width"] = Value::from(filled - taken);
            remaining -= taken;
        }
        0
    };
    let delta = i64::from(report.percent) - prior_percent as i64;
    segments.push(json!({
        "stage":report.stage,
        "width":width,
        "note":report.note,
        "visit_at":at,
        "reported_percent":report.percent,
        "delta":delta
    }));
    json!({
        "updated_at":at,
        "percent":report.percent,
        "stage":report.stage,
        "note":report.note,
        "segments":segments
    })
}

/// Read the current percent and segments, refusing anything that would make
/// the CLI invent or repair history.
fn carried_progress(message: &Value) -> Result<(u64, Vec<Value>), CommandError> {
    let unavailable = |detail: &str| {
        CommandError::new(
            format!(
                "cannot prepare a Progress update: the message's current progress {detail}; no update was sent"
            ),
            1,
        )
    };
    let progress = message
        .get("progress")
        .and_then(Value::as_object)
        .ok_or_else(|| unavailable("member is missing"))?;
    let percent = progress
        .get("percent")
        .and_then(Value::as_u64)
        .filter(|percent| *percent <= 100)
        .ok_or_else(|| unavailable("percent is not a whole number from 0 to 100"))?;
    let segments = progress
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| unavailable("segment list is missing"))?;
    let mut total = 0u64;
    for segment in segments {
        let width = segment
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| unavailable("has a segment without a whole-number width"))?;
        if !segment.get("stage").is_some_and(Value::is_string) {
            return Err(unavailable("has a segment without a stage"));
        }
        total += width;
    }
    if total != percent {
        return Err(unavailable("segment widths do not add up to its percent"));
    }
    Ok((percent, segments.clone()))
}

/// Update an existing Progress message in place through the ordinary revise
/// path, using the locally retained current revision of that message.
pub fn update(
    context: &CommandContext,
    session: Uuid,
    message_id: &str,
    report: &ProgressReport,
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    let mut store = context.open_store(session)?;
    let op_id = op_id.unwrap_or_else(Uuid::new_v4);
    let response = update_with(
        &context.api,
        &mut store,
        session,
        message_id,
        report,
        op_id,
        now_ms(),
    )?;
    emit_committed("revise", op_id, response)
}

fn update_with(
    transport: &impl OperationTransport,
    store: &mut JournalStore,
    session: Uuid,
    message_id: &str,
    report: &ProgressReport,
    op_id: Uuid,
    now: u64,
) -> Result<Value, CommandError> {
    report.validate()?;
    // Never stage into an existing operation's directory. Keep the exit class
    // callers already rely on: an unresolved key means "retry it", and a
    // settled key is the same conflict the ordinary submit path reports.
    if let Some(existing) = store.operation(op_id).map_err(|e| journal_error(e, 1))? {
        if matches!(
            existing.state,
            crate::journal::OperationState::Prepared | crate::journal::OperationState::Unknown
        ) {
            return Err(CommandError::new(
                format!("request has unresolved operation {op_id}; retry it before continuing"),
                super::EXIT_OUTCOME_UNKNOWN,
            ));
        }
        return Err(journal_error(
            crate::journal::JournalError::OperationConflict(op_id),
            1,
        ));
    }
    let snapshot = store
        .request_snapshot(session, message_id, None)
        .map_err(|error| journal_error(error, 1))?;
    let request = request_context(&snapshot, session, message_id)?;
    ensure_request_open(&snapshot, session, &request)?;
    let is_progress = request.message.get("message_type").and_then(Value::as_str) == Some("fyi")
        && request.message.get("progress").is_some();
    if !is_progress {
        return Err(CommandError::new(
            format!(
                "message {message_id} is not a Progress message; only a message sent with --progress can be updated this way"
            ),
            EXIT_REFUSED,
        ));
    }

    // A revise is a full replacement: repeat every current field, including
    // the complete progress member, and change only progress and the time.
    let mut message = request.message.clone();
    message["progress"] = next_progress(&request.message, report, now)?;
    message["created_at"] = Value::from(now);
    message["sender_id"] = Value::String(request.sender_id.clone());
    message["message_id"] = Value::String(request.message_id.clone());

    let mut command = json!({
        "v":1,
        "type":"revise",
        "op_id":op_id.to_string(),
        "sender_id":request.sender_id,
        "message_id":request.message_id,
        "base_revision":request.revision,
        "message":message
    });
    let prepared = preserved_files(transport, store, &snapshot, &request, session, op_id)?;
    command["files"] = Value::Array(prepared.entries);
    let uploads = prepared.uploads;
    let operation =
        super::prepare_with_uploads(store, session, op_id, "revise", command, uploads.clone())
            .map_err(|error| {
                if store.operation(op_id).ok().flatten().is_none() {
                    files::cleanup_staged_uploads(&uploads);
                }
                error
            })?;
    submit_operation_with_transport(transport, store, operation)
}

fn preserved_files(
    transport: &impl OperationTransport,
    store: &JournalStore,
    snapshot: &crate::journal::Journal,
    request: &super::RequestContext,
    session: Uuid,
    op_id: Uuid,
) -> Result<PreparedFiles, CommandError> {
    let unavailable = |reason: String| {
        CommandError::new(
            format!(
                "cannot prepare a Progress update: current revision file bytes are unavailable ({reason}); no update was sent"
            ),
            EXIT_REFUSED,
        )
    };
    let entries = request
        .files
        .as_array()
        .ok_or_else(|| unavailable("invalid retained manifest".into()))?;
    let identity = files::retained_manifest(entries).map_err(&unavailable)?;
    let mut captured = entries.clone();
    let mut sources = vec![];
    // A feed snapshot wins a same-revision tie for message content. Only a
    // matching committed operation of THAT revision can supply absent bytes.
    for op in snapshot.operations.values().filter(|op| {
        op.state == crate::journal::OperationState::Committed
            && op.session_handle == session
            && op.request["message"]["sender_id"] == request.sender_id
            && op.request["message"]["message_id"] == request.message_id
            && crate::journal::operation_request_revision(op)
                == i64::try_from(request.revision).ok()
    }) {
        let empty = vec![];
        let prior = match op.request.get("files") {
            None => &empty,
            Some(value) => value
                .as_array()
                .ok_or_else(|| unavailable("invalid operation manifest".into()))?,
        };
        if files::retained_manifest(prior).ok().as_ref() != Some(&identity)
            || !entries.iter().zip(prior).all(|(a, b)| {
                a.get("upload_id")
                    .is_none_or(|id| b.get("upload_id") == Some(id))
            })
        {
            return Err(unavailable("same-revision manifests disagree".into()));
        }
        for (file, original) in captured.iter_mut().zip(prior) {
            if file.get("data_base64").is_none() && original.get("data_base64").is_some() {
                *file = original.clone();
            }
        }
        sources = op.uploads.clone();
    }
    if entries.is_empty() {
        return Ok(PreparedFiles {
            entries: vec![],
            uploads: vec![],
            manifest: vec![],
        });
    }
    // Validate availability before even consulting limits. Fingerprints alone
    // are not source bytes, and a former upload ID is not new upload authority.
    for (file, identity) in captured.iter().zip(&identity) {
        if file.get("data_base64").is_none()
            && !sources.iter().any(|source| {
                source.name == identity.name
                    && source.size_bytes == identity.size_bytes
                    && source.sha256 == identity.sha256
                    && source.staged_path.is_some()
            })
        {
            return Err(unavailable(identity.name.clone()));
        }
    }
    let response = transport
        .post(&json!({"v":1,"type":"file_limits"}))
        .map_err(|error| unavailable(error.to_string()))?;
    let limits = ServerFileLimits::from_response(&response).map_err(&unavailable)?;
    files::prepare_retained(
        &captured,
        &sources,
        &limits,
        &store.upload_staging_directory(session, op_id),
    )
    .map_err(unavailable)
}

/// RFC 3339 UTC with millisecond precision, e.g. `2026-11-09T10:00:00.000Z`.
pub(super) fn rfc3339_utc(ms: u64) -> String {
    let seconds = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, path::Path, sync::Mutex};

    use super::super::{MessageFieldOptions, build_send_request_with_fields, prepare};
    use super::*;
    use crate::api::ApiError;
    use crate::files::{self, ServerFileLimits};
    use crate::journal::{JournalEntry, OperationState};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};

    const SENDER: &str = "app:example";
    const MESSAGE: &str = "progress-1";

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("harklight-agent-progress-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Records each posted request and answers from a queue, defaulting to
    /// the ordinary success envelope with the next revision number.
    struct Origin {
        posts: Mutex<Vec<Value>>,
        answers: Mutex<VecDeque<Result<Value, ApiError>>>,
        uploads: Mutex<Vec<Value>>,
    }

    impl Origin {
        fn new() -> Self {
            Self {
                posts: Mutex::new(Vec::new()),
                answers: Mutex::new(VecDeque::new()),
                uploads: Mutex::new(Vec::new()),
            }
        }

        fn then(self, answer: Result<Value, ApiError>) -> Self {
            self.answers.lock().unwrap().push_back(answer);
            self
        }

        fn last_post(&self) -> Value {
            self.posts.lock().unwrap().last().unwrap().clone()
        }
    }

    impl OperationTransport for Origin {
        fn post(&self, request: &Value) -> Result<Value, ApiError> {
            if request["type"] == "file_limits" {
                return Ok(
                    json!({"type":"ok","inline_decoded_bytes_per_entry":262144,"upload_cap_bytes_per_file":"none"}),
                );
            }
            let mut posts = self.posts.lock().unwrap();
            posts.push(request.clone());
            let revision = posts.len() as u64;
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"v":1,"type":"ok","revision":revision})))
        }

        fn upload_file(
            &self,
            op_id: Uuid,
            sender: &str,
            message: &str,
            revision: u64,
            name: &str,
            path: &Path,
            size: u64,
        ) -> Result<Value, ApiError> {
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.len() as u64, size);
            let response = json!({"type":"ok","name":name,"size":size,
                "sha256":hex::encode(Sha256::digest(bytes)),"upload_id":Uuid::new_v4().to_string()});
            self.uploads
                .lock()
                .unwrap()
                .push(json!({"op_id":op_id.to_string(),"sender_id":sender,
                "message_id":message,"revision":revision,"response":response}));
            Ok(response)
        }
    }

    fn report(percent: u8, stage: &str, note: Option<&str>) -> ProgressReport {
        ProgressReport {
            percent,
            stage: stage.into(),
            note: note.map(str::to_owned),
        }
    }

    fn send_progress(
        origin: &Origin,
        store: &mut JournalStore,
        session: Uuid,
        first: ProgressReport,
        at: u64,
    ) -> Value {
        let op_id = Uuid::new_v4();
        let fields = MessageFieldOptions {
            message_type: Some("fyi".into()),
            progress: Some(first),
            ..Default::default()
        };
        let mut request = build_send_request_with_fields(
            op_id,
            session,
            MESSAGE,
            SENDER,
            "Ship the release",
            None,
            &[],
            None,
            None,
            false,
            &[],
            &fields,
        )
        .unwrap();
        // Fix the preparation instant so the example's times are exact.
        request["message"]["created_at"] = Value::from(at);
        request["message"]["progress"] = initial_progress(fields.progress.as_ref().unwrap(), at);
        let operation = prepare(store, session, op_id, "send", request).unwrap();
        submit_operation_with_transport(origin, store, operation).unwrap();
        origin.last_post()
    }

    fn widths(post: &Value) -> Vec<u64> {
        post["message"]["progress"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|segment| segment["width"].as_u64().unwrap())
            .collect()
    }

    fn stages(post: &Value) -> Vec<String> {
        post["message"]["progress"]["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|segment| segment["stage"].as_str().unwrap().to_owned())
            .collect()
    }

    const T0: u64 = 1_794_214_800_000; // 2026-11-09T09:00:00.000Z

    fn send_file(
        origin: &Origin,
        store: &mut JournalStore,
        session: Uuid,
        root: &Path,
        uploaded: bool,
    ) -> Uuid {
        let path = root.join("input.bin");
        std::fs::write(&path, [1, 2, 3, 4]).unwrap();
        let op = Uuid::new_v4();
        let prepared = files::prepare_outgoing(
            &[path],
            &ServerFileLimits {
                inline_decoded_bytes_per_entry: if uploaded { 0 } else { 262144 },
                upload_cap_bytes_per_file: None,
            },
            &store.upload_staging_directory(session, op),
        )
        .unwrap();
        let fields = MessageFieldOptions {
            message_type: Some("fyi".into()),
            progress: Some(report(10, "coding", Some("first"))),
            ..Default::default()
        };
        let mut request = build_send_request_with_fields(
            op,
            session,
            MESSAGE,
            SENDER,
            "Keep title",
            Some("Keep body"),
            &[],
            None,
            None,
            false,
            &prepared.entries,
            &fields,
        )
        .unwrap();
        request["message"]["extra"] = json!({"keep":[1,"two"]});
        request["message"]["urgency"] = json!("high");
        request["message"]["thread"] = json!("release");
        request["message"]["progress"] = initial_progress(fields.progress.as_ref().unwrap(), T0);
        let operation = super::super::prepare_with_uploads(
            store,
            session,
            op,
            "send",
            request,
            prepared.uploads,
        )
        .unwrap();
        submit_operation_with_transport(origin, store, operation).unwrap();
        op
    }

    fn revision_entry(
        session: Uuid,
        seq: u64,
        revision: u64,
        message: Value,
        files: Value,
    ) -> JournalEntry {
        JournalEntry::fetched(
            seq,
            seq.to_string(),
            session,
            "revision".into(),
            "origin".into(),
            false,
            Some(SENDER.into()),
            Some(MESSAGE.into()),
            Some(revision),
            json!({"kind":"revision","author":"origin",
                "sender_id":SENDER,"message_id":MESSAGE,"request_revision":revision,"message":message,"files":files}),
        )
    }

    #[test]
    fn inline_files_survive_revisions_unknown_replay_and_reference_cleanup() {
        file_preservation_case(false);
    }

    #[test]
    fn uploaded_files_survive_revisions_unknown_replay_and_reference_cleanup() {
        file_preservation_case(true);
    }

    fn file_preservation_case(uploaded: bool) {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new();
        let first = send_file(&origin, &mut store, session, &scratch.0, uploaded);
        let original = origin.last_post();
        let first_op = store.operation(first).unwrap().unwrap();
        let old_path = first_op.uploads.first().and_then(|u| u.staged_path.clone());
        if let Some(path) = &old_path {
            assert!(path.exists());
        }
        // The caller's mutable input is no longer the authoritative source.
        std::fs::write(scratch.0.join("input.bin"), [9, 9]).unwrap();
        let second = Uuid::new_v4();
        update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(20, "review", Some("second")),
            second,
            T0 + 1,
        )
        .unwrap();
        let wire = origin.last_post();
        assert_eq!(wire["base_revision"], 1);
        assert_eq!(wire["op_id"], second.to_string());
        assert!(
            wire.get("files").is_some(),
            "Progress revise omitted the current file manifest"
        );
        assert_eq!(wire["files"].as_array().unwrap().len(), 1);
        let mut before = original["message"].clone();
        let mut after = wire["message"].clone();
        for key in ["progress", "created_at"] {
            before.as_object_mut().unwrap().remove(key);
            after.as_object_mut().unwrap().remove(key);
        }
        assert_eq!(before, after);
        assert_eq!(
            wire["message"]["progress"]["segments"][0],
            original["message"]["progress"]["segments"][0]
        );
        assert_eq!(
            wire["message"]["progress"]["segments"][1]["visit_at"],
            rfc3339_utc(T0 + 1)
        );
        if uploaded {
            assert_ne!(
                wire["files"][0]["upload_id"],
                original["files"][0]["upload_id"]
            );
            let uploads = origin.uploads.lock().unwrap();
            assert_eq!(uploads[1]["op_id"], second.to_string());
            assert_eq!(uploads[1]["revision"], 2);
            assert_eq!(uploads[1]["sender_id"], SENDER);
            assert_eq!(uploads[1]["message_id"], MESSAGE);
            assert_eq!(
                wire["files"][0]["sha256"],
                hex::encode(Sha256::digest([1, 2, 3, 4]))
            );
        } else {
            assert_eq!(wire["files"], original["files"]);
        }
        if let Some(path) = &old_path {
            assert!(!path.exists());
        }
        assert!(
            store.operation(first).unwrap().unwrap().request["files"][0]
                .get("data_base64")
                .is_none()
        );
        let second_op = store.operation(second).unwrap().unwrap();
        let current_path = second_op
            .uploads
            .first()
            .and_then(|u| u.staged_path.clone());

        let third = Uuid::new_v4();
        origin
            .answers
            .lock()
            .unwrap()
            .push_back(Err(ApiError::InvalidResponse("lost".into())));
        assert_eq!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(30, "testing", None),
                third,
                T0 + 2
            )
            .unwrap_err()
            .exit_code,
            super::super::EXIT_OUTCOME_UNKNOWN
        );
        let uncertain_wire = origin.last_post();
        let pending = store.operation(third).unwrap().unwrap();
        assert_eq!(pending.state, OperationState::Unknown);
        assert_eq!(pending.request, uncertain_wire);
        if let Some(path) = &current_path {
            assert!(path.exists());
        }
        let upload_count = origin.uploads.lock().unwrap().len();
        drop(store);
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let pending = store.operation(third).unwrap().unwrap();
        assert_eq!(pending.request, uncertain_wire);
        origin
            .answers
            .lock()
            .unwrap()
            .push_back(Ok(json!({"type":"ok","revision":3,"duplicate":true})));
        let receipt = submit_operation_with_transport(&origin, &mut store, pending).unwrap();
        assert_eq!(origin.last_post(), uncertain_wire);
        assert_eq!(origin.uploads.lock().unwrap().len(), upload_count);
        if let Some(path) = &current_path {
            assert!(!path.exists());
        }
        let committed = store.operation(third).unwrap().unwrap();
        let posts = origin.posts.lock().unwrap().len();
        assert_eq!(
            submit_operation_with_transport(&origin, &mut store, committed.clone()).unwrap(),
            receipt
        );
        assert_eq!(origin.posts.lock().unwrap().len(), posts);
        let mut changed = committed.request.clone();
        changed["message"]["title"] = json!("changed");
        assert!(
            store
                .prepare_operation(third, session, "revise".into(), changed)
                .is_err()
        );

        // Closure does not release either revision's bytes while an
        // unresolved successor still references the accepted base.
        let fourth = Uuid::new_v4();
        origin
            .answers
            .lock()
            .unwrap()
            .push_back(Err(ApiError::InvalidResponse("lost".into())));
        assert!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(40, "coding", None),
                fourth,
                T0 + 3
            )
            .is_err()
        );
        let terminal = JournalEntry::fetched(
            1,
            "1".into(),
            session,
            "terminal".into(),
            "person".into(),
            true,
            Some(SENDER.into()),
            Some(MESSAGE.into()),
            Some(3),
            json!({"terminal_state":"cleared"}),
        );
        store.record_entry(terminal).unwrap();
        let base = store.operation(third).unwrap().unwrap();
        if uploaded {
            assert!(base.uploads[0].staged_path.as_ref().unwrap().exists());
        } else {
            assert!(base.request["files"][0].get("data_base64").is_some());
        }
        let fourth_paths = store.operation(fourth).unwrap().unwrap().uploads;
        store
            .reject_operation(fourth, json!({"type":"error","reason":"closed"}))
            .unwrap();
        let released = store.operation(third).unwrap().unwrap();
        assert_eq!(released.response, Some(receipt));
        assert!(released.request["files"][0].get("data_base64").is_none());
        for u in base.uploads.iter().chain(&fourth_paths) {
            if let Some(path) = &u.staged_path {
                assert!(!path.exists());
            }
        }
        assert_eq!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(50, "review", None),
                Uuid::new_v4(),
                T0 + 4
            )
            .unwrap_err()
            .exit_code,
            EXIT_REFUSED
        );
    }

    #[test]
    fn committed_withdraw_releases_current_inline_and_uploaded_material() {
        for uploaded in [false, true] {
            let scratch = Scratch::new();
            let session = Uuid::new_v4();
            let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
            let origin = Origin::new();
            let first = send_file(&origin, &mut store, session, &scratch.0, uploaded);
            let kept = store.operation(first).unwrap().unwrap();
            let staged = kept.uploads.first().and_then(|u| u.staged_path.clone());
            if let Some(path) = &staged {
                assert!(path.exists());
            } else {
                assert!(kept.request["files"][0].get("data_base64").is_some());
            }
            let withdraw = Uuid::new_v4();
            let operation = store
                .prepare_operation(
                    withdraw,
                    session,
                    "withdraw".into(),
                    json!({"v":1,"type":"withdraw","op_id":withdraw.to_string(),
                        "sender_id":SENDER,"message_id":MESSAGE}),
                )
                .unwrap();
            submit_operation_with_transport(&origin, &mut store, operation).unwrap();
            let released = store.operation(first).unwrap().unwrap();
            assert_eq!(released.response, kept.response);
            assert!(released.request["files"][0].get("data_base64").is_none());
            assert!(released.uploads.iter().all(|u| u.staged_path.is_none()));
            if let Some(path) = &staged {
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn reused_update_key_keeps_its_retry_or_conflict_exit_class() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new();
        send_progress(&origin, &mut store, session, report(10, "coding", None), T0);
        let committed = Uuid::new_v4();
        update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(20, "coding", None),
            committed,
            T0 + 1,
        )
        .unwrap();
        let again = update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(20, "coding", None),
            committed,
            T0 + 2,
        )
        .unwrap_err();
        assert_eq!(again.exit_code, 1);

        let lost = Origin::new().then(Err(ApiError::InvalidResponse("lost".into())));
        let unknown = Uuid::new_v4();
        update_with(
            &lost,
            &mut store,
            session,
            MESSAGE,
            &report(30, "coding", None),
            unknown,
            T0 + 3,
        )
        .unwrap_err();
        let staged = store.operation(unknown).unwrap().unwrap().request.clone();
        let retry = update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(30, "coding", None),
            unknown,
            T0 + 4,
        )
        .unwrap_err();
        assert_eq!(retry.exit_code, super::super::EXIT_OUTCOME_UNKNOWN);
        assert!(retry.message.contains(&unknown.to_string()));
        assert_eq!(store.operation(unknown).unwrap().unwrap().request, staged);
    }

    #[test]
    fn current_feed_revision_and_manifest_are_selected_together_without_older_fallback() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let origin = Origin::new();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        send_file(&origin, &mut store, session, &scratch.0, false);
        let mut message = origin.last_post()["message"].clone();
        message["title"] = json!("new current title");
        let files = json!([{"name":"new.bin","data_base64":STANDARD.encode([5,6])}]);
        store
            .record_entry(revision_entry(
                session,
                1,
                2,
                message.clone(),
                files.clone(),
            ))
            .unwrap();
        origin
            .answers
            .lock()
            .unwrap()
            .push_back(Ok(json!({"type":"ok","revision":3})));
        update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(20, "review", None),
            Uuid::new_v4(),
            T0 + 1,
        )
        .unwrap();
        let wire = origin.last_post();
        assert_eq!(wire["base_revision"], 2);
        assert_eq!(wire["message"]["title"], "new current title");
        assert_eq!(wire["files"], files);
        // A newer metadata-only snapshot cannot borrow the old source bytes.
        store
            .record_entry(revision_entry(
                session,
                2,
                4,
                message,
                json!([{"name":"new.bin","tier":"inline","size":2,
            "sha256":hex::encode(Sha256::digest([7,8]))}]),
            ))
            .unwrap();
        let posts = origin.posts.lock().unwrap().len();
        let op = Uuid::new_v4();
        assert!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(30, "review", None),
                op,
                T0 + 2
            )
            .unwrap_err()
            .message
            .contains("file bytes are unavailable")
        );
        assert!(store.operation(op).unwrap().is_none());
        assert_eq!(origin.posts.lock().unwrap().len(), posts);
    }

    #[test]
    fn same_revision_feed_metadata_uses_only_matching_captured_material() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let origin = Origin::new();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        send_file(&origin, &mut store, session, &scratch.0, false);
        let initial = origin.last_post();
        let metadata = json!([{"name":"input.bin","tier":"inline","size":4,"sha256":hex::encode(Sha256::digest([1,2,3,4]))}]);
        store
            .record_entry(revision_entry(
                session,
                1,
                1,
                initial["message"].clone(),
                metadata.clone(),
            ))
            .unwrap();
        update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(20, "review", None),
            Uuid::new_v4(),
            T0 + 1,
        )
        .unwrap();
        assert_eq!(origin.last_post()["files"], initial["files"]);
        let mut wrong = metadata;
        wrong[0]["sha256"] = json!(hex::encode(Sha256::digest([9, 9, 9, 9])));
        store
            .record_entry(revision_entry(
                session,
                2,
                2,
                origin.last_post()["message"].clone(),
                wrong,
            ))
            .unwrap();
        assert!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(30, "review", None),
                Uuid::new_v4(),
                T0 + 2
            )
            .unwrap_err()
            .message
            .contains("manifests disagree")
        );
    }

    #[test]
    fn missing_or_changed_captured_upload_and_missing_snapshot_refuse_without_posting() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let origin = Origin::new();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        assert!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(10, "review", None),
                Uuid::new_v4(),
                T0
            )
            .is_err()
        );
        let first = send_file(&origin, &mut store, session, &scratch.0, true);
        let source = store.operation(first).unwrap().unwrap().uploads[0]
            .staged_path
            .clone()
            .unwrap();
        let posts = origin.posts.lock().unwrap().len();
        let uploads = origin.uploads.lock().unwrap().len();
        std::fs::write(&source, [9, 9, 9, 9]).unwrap();
        assert!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(20, "review", None),
                Uuid::new_v4(),
                T0 + 1
            )
            .is_err()
        );
        std::fs::remove_file(source).unwrap();
        assert!(
            update_with(
                &origin,
                &mut store,
                session,
                MESSAGE,
                &report(20, "review", None),
                Uuid::new_v4(),
                T0 + 2
            )
            .is_err()
        );
        assert_eq!(origin.posts.lock().unwrap().len(), posts);
        assert_eq!(origin.uploads.lock().unwrap().len(), uploads);
    }

    #[test]
    fn stale_file_update_releases_only_the_refused_candidate() {
        for uploaded in [false, true] {
            let scratch = Scratch::new();
            let session = Uuid::new_v4();
            let origin = Origin::new();
            let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
            let first = send_file(&origin, &mut store, session, &scratch.0, uploaded);
            let current = store.operation(first).unwrap().unwrap();
            origin
                .answers
                .lock()
                .unwrap()
                .push_back(Ok(json!({"type":"error","reason":"stale_revision",
                "field":"base_revision","current_revision":2})));
            let candidate = Uuid::new_v4();
            assert_eq!(
                update_with(
                    &origin,
                    &mut store,
                    session,
                    MESSAGE,
                    &report(20, "review", None),
                    candidate,
                    T0 + 1
                )
                .unwrap_err()
                .exit_code,
                EXIT_REFUSED
            );
            let refused = store.operation(candidate).unwrap().unwrap();
            assert_eq!(refused.state, OperationState::Rejected);
            assert!(refused.request["files"][0].get("data_base64").is_none());
            assert!(refused.uploads.iter().all(|u| u.staged_path.is_none()));
            let after = store.operation(first).unwrap().unwrap();
            assert_eq!(after.request, current.request);
            assert_eq!(after.response, current.response);
            for u in &after.uploads {
                assert!(u.staged_path.as_ref().unwrap().exists());
            }
        }
    }

    #[test]
    fn send_creates_an_ordinary_fyi_carrying_the_first_visit() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new();

        let post = send_progress(
            &origin,
            &mut store,
            session,
            report(10, "spec_writing", Some("Scope agreed")),
            T0,
        );

        assert_eq!(post["type"], "send");
        let message = &post["message"];
        assert_eq!(message["message_type"], "fyi");
        assert_eq!(
            message["answer_ui"],
            json!({"schema":"attention-client.actions/1","actions":[{"id":"seen","label":"Seen","kind":"value","value":{"acknowledgment":"seen"}}]})
        );
        assert_eq!(message["sender_id"], SENDER);
        assert_eq!(message["message_id"], MESSAGE);
        assert_eq!(message["title"], "Ship the release");
        assert_eq!(
            message["progress"],
            json!({
                "updated_at":"2026-11-09T09:00:00.000Z",
                "percent":10,
                "stage":"spec_writing",
                "note":"Scope agreed",
                "segments":[{
                    "stage":"spec_writing","width":10,"note":"Scope agreed",
                    "visit_at":"2026-11-09T09:00:00.000Z","reported_percent":10,"delta":10
                }]
            })
        );
    }

    #[test]
    fn updates_follow_the_contract_example_through_ordinary_revises() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new();
        send_progress(
            &origin,
            &mut store,
            session,
            report(10, "spec_writing", Some("Scope agreed")),
            T0,
        );

        let mut step = 0u64;
        let mut update = |store: &mut JournalStore, percent, stage, note| {
            step += 1;
            update_with(
                &origin,
                store,
                session,
                MESSAGE,
                &report(percent, stage, note),
                Uuid::new_v4(),
                T0 + step * 60_000,
            )
            .unwrap();
            origin.last_post()
        };

        // spec10, coding40, review40, coding60 -> 10,30,0,20.
        update(&mut store, 40, "coding", None);
        let post = update(&mut store, 40, "review", Some("Checking the result"));
        assert_eq!(widths(&post), vec![10, 30, 0]);
        let post = update(&mut store, 60, "coding", None);
        assert_eq!(widths(&post), vec![10, 30, 0, 20]);
        assert_eq!(
            stages(&post),
            ["spec_writing", "coding", "review", "coding"]
        );

        // The ordinary revise wire, repeating the full message.
        assert_eq!(post["type"], "revise");
        assert_eq!(post["sender_id"], SENDER);
        assert_eq!(post["message_id"], MESSAGE);
        assert_eq!(post["base_revision"], 3);
        assert_eq!(post["message"]["message_type"], "fyi");
        assert_eq!(post["message"]["title"], "Ship the release");
        assert_eq!(post["message"]["answer_ui"]["actions"][0]["id"], "seen");
        assert_eq!(post["message"]["progress"]["percent"], 60);
        assert_eq!(post["message"]["progress"]["stage"], "coding");
        assert_eq!(post["message"]["progress"]["note"], Value::Null);

        // Lower to 45 in testing: trim the newest width, keep the review note.
        let post = update(&mut store, 45, "testing", Some("Found a regression"));
        assert_eq!(widths(&post), vec![10, 30, 0, 5, 0]);
        let segments = post["message"]["progress"]["segments"].as_array().unwrap();
        assert_eq!(segments[2]["note"], "Checking the result");
        assert_eq!(segments[4]["note"], "Found a regression");
        assert_eq!(segments[4]["reported_percent"], 45);
        assert_eq!(segments[4]["delta"], -15);

        // Lower again to 5: earlier widths become 5,0,0,0,0 plus a new visit.
        let post = update(&mut store, 5, "testing", None);
        assert_eq!(widths(&post), vec![5, 0, 0, 0, 0, 0]);
        let segments = post["message"]["progress"]["segments"].as_array().unwrap();
        // Trimming changes widths only; earlier visit metadata survives.
        assert_eq!(segments[3]["reported_percent"], 60);
        assert_eq!(segments[3]["delta"], 20);
        assert_eq!(segments[3]["visit_at"], "2026-11-09T09:03:00.000Z");
        assert_eq!(segments[4]["reported_percent"], 45);
        assert_eq!(segments[4]["delta"], -15);
        assert_eq!(segments[5]["delta"], -40);

        // Raising later to 20 in review appends 15 review.
        let post = update(&mut store, 20, "review", None);
        assert_eq!(widths(&post), vec![5, 0, 0, 0, 0, 0, 15]);
        assert_eq!(stages(&post).last().unwrap(), "review");
        assert_eq!(
            post["message"]["progress"]["updated_at"],
            "2026-11-09T09:06:00.000Z"
        );
        let total: u64 = widths(&post).iter().sum();
        assert_eq!(total, 20);
    }

    #[test]
    fn a_stale_refusal_leaves_the_bar_unapplied() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new().then(Ok(json!({"v":1,"type":"ok","revision":1})));
        send_progress(&origin, &mut store, session, report(10, "coding", None), T0);
        let stale = Origin::new().then(Ok(json!({
            "v":1,"type":"error","reason":"stale_revision","field":"base_revision","current_revision":2
        })));

        let error = update_with(
            &stale,
            &mut store,
            session,
            MESSAGE,
            &report(50, "coding", None),
            Uuid::new_v4(),
            T0 + 1,
        )
        .unwrap_err();
        assert_eq!(error.exit_code, EXIT_REFUSED);

        // The next update still builds from the last accepted revision.
        let next = Origin::new();
        update_with(
            &next,
            &mut store,
            session,
            MESSAGE,
            &report(20, "coding", None),
            Uuid::new_v4(),
            T0 + 2,
        )
        .unwrap();
        let post = next.last_post();
        assert_eq!(post["base_revision"], 1);
        assert_eq!(widths(&post), vec![10, 10]);
    }

    #[test]
    fn an_unknown_outcome_keeps_the_exact_operation_for_retry() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new();
        send_progress(&origin, &mut store, session, report(10, "coding", None), T0);
        let lost = Origin::new().then(Err(ApiError::InvalidResponse("response was lost".into())));
        let op_id = Uuid::new_v4();

        let error = update_with(
            &lost,
            &mut store,
            session,
            MESSAGE,
            &report(40, "coding", None),
            op_id,
            T0 + 1,
        )
        .unwrap_err();
        assert_eq!(error.exit_code, super::super::EXIT_OUTCOME_UNKNOWN);
        let sent = lost.last_post();

        // A new update is blocked until the uncertain one is retried.
        let blocked = update_with(
            &Origin::new(),
            &mut store,
            session,
            MESSAGE,
            &report(50, "coding", None),
            Uuid::new_v4(),
            T0 + 2,
        )
        .unwrap_err();
        assert!(blocked.message.contains(&op_id.to_string()));

        // Retrying resubmits the identical saved operation, segments and time.
        let retry = Origin::new();
        let operation = store.operation(op_id).unwrap().unwrap();
        submit_operation_with_transport(&retry, &mut store, operation).unwrap();
        assert_eq!(retry.last_post(), sent);
    }

    #[test]
    fn only_a_progress_message_can_be_updated() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let origin = Origin::new();
        let op_id = Uuid::new_v4();
        let request = build_send_request_with_fields(
            op_id,
            session,
            MESSAGE,
            SENDER,
            "Approve the plan?",
            None,
            &["Approve".to_string()],
            None,
            None,
            false,
            &[],
            &MessageFieldOptions {
                message_type: Some("request".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let operation = prepare(&mut store, session, op_id, "send", request).unwrap();
        submit_operation_with_transport(&origin, &mut store, operation).unwrap();

        let error = update_with(
            &origin,
            &mut store,
            session,
            MESSAGE,
            &report(50, "coding", None),
            Uuid::new_v4(),
            T0,
        )
        .unwrap_err();
        assert_eq!(error.exit_code, EXIT_REFUSED);
        assert_eq!(origin.posts.lock().unwrap().len(), 1);
    }

    #[test]
    fn malformed_carried_progress_is_not_repaired() {
        let current = json!({"progress":{"percent":40,"segments":[{"stage":"coding","width":30}]}});
        let error = next_progress(&current, &report(50, "coding", None), T0).unwrap_err();
        assert!(error.message.contains("do not add up"));
        let current = json!({"progress":{"percent":40}});
        assert!(next_progress(&current, &report(50, "coding", None), T0).is_err());
    }

    #[test]
    fn progress_requires_an_fyi() {
        let send = |message_type: &str, stage: &str| {
            build_send_request_with_fields(
                Uuid::new_v4(),
                Uuid::new_v4(),
                MESSAGE,
                SENDER,
                "Title",
                None,
                &[],
                None,
                None,
                false,
                &[],
                &MessageFieldOptions {
                    message_type: Some(message_type.into()),
                    progress: Some(report(10, stage, None)),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            send("request", "coding").unwrap_err().exit_code,
            EXIT_REFUSED
        );
        assert!(send("fyi", "packaging").is_ok());
    }

    #[test]
    fn update_times_are_rfc3339_utc_milliseconds() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_utc(T0 + 3_600_123), "2026-11-09T10:00:00.123Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
