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
    ensure_request_open, journal_error, now_ms, prepare, request_context,
    submit_operation_with_transport, validate_request_pre_upload,
};
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
        if self.stage.trim().is_empty() {
            return Err(CommandError::new(
                "progress stage must be a nonblank label",
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

    let command = json!({
        "v":1,
        "type":"revise",
        "op_id":op_id.to_string(),
        "sender_id":request.sender_id,
        "message_id":request.message_id,
        "base_revision":request.revision,
        "message":message
    });
    validate_request_pre_upload(&command)?;
    let operation = prepare(store, session, op_id, "revise", command)?;
    submit_operation_with_transport(transport, store, operation)
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

    use super::super::{MessageFieldOptions, build_send_request_with_fields};
    use super::*;
    use crate::api::ApiError;

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
    }

    impl Origin {
        fn new() -> Self {
            Self {
                posts: Mutex::new(Vec::new()),
                answers: Mutex::new(VecDeque::new()),
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
            _op_id: Uuid,
            _sender_id: &str,
            _message_id: &str,
            _request_revision: u64,
            _name: &str,
            _path: &Path,
            _size: u64,
        ) -> Result<Value, ApiError> {
            unreachable!("Progress messages carry no files")
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
            false,
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
            false,
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
    fn progress_requires_an_fyi_and_a_nonblank_stage() {
        let send = |message_type: &str, stage: &str| {
            build_send_request_with_fields(
                Uuid::new_v4(),
                Uuid::new_v4(),
                MESSAGE,
                SENDER,
                "Title",
                None,
                &[],
                false,
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
        assert_eq!(send("fyi", "  ").unwrap_err().exit_code, EXIT_REFUSED);
        assert!(send("fyi", "packaging").is_ok());
    }

    #[test]
    fn update_times_are_rfc3339_utc_milliseconds() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_utc(T0 + 3_600_123), "2026-11-09T10:00:00.123Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
