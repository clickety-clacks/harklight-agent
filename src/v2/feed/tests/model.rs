use super::*;
use crate::test_tls::RecordedRequest;
use std::sync::{Arc, Mutex};

const PIN: u64 = 40;
const FUTURE: u64 = 100;

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    PrepareResponse,
    SecondPageResponse,
    OpeningRecord,
    ChatRecord,
    CompletionRecord,
    CompletionFlush,
    AfterCompletion,
    CaughtUpRecord,
}

struct Turn {
    row: Value,
    served_by: Option<usize>,
}

struct Token {
    pin: u64,
    session: Option<String>,
}

// The Server marks first service before attempting a page response. Scan tokens
// own stable turns and can replay them even after their ordinary feed pin expires.
struct Server {
    head: u64,
    floor: u64,
    turns: Vec<Turn>,
    tokens: Vec<Token>,
    requests: Vec<(String, bool)>,
    fault: Fault,
    unsupported: bool,
    expire_on_stream: bool,
}

fn sse_closing(reason: &str) -> FixtureResponse {
    FixtureResponse {
        status: "200 OK",
        headers: &[("Content-Type", "text/event-stream")],
        body: format!("event: closing\ndata: {{\"reason\":\"{reason}\"}}\n\n"),
        truncate: false,
    }
}

impl Server {
    fn new(session: Uuid) -> Self {
        let (chat, answer) = retained_turns(session);
        Self {
            head: PIN,
            floor: 30,
            turns: [chat, answer]
                .into_iter()
                .map(|row| Turn {
                    row,
                    served_by: None,
                })
                .collect(),
            tokens: Vec::new(),
            requests: Vec::new(),
            fault: Fault::None,
            unsupported: false,
            expire_on_stream: false,
        }
    }

    fn respond(&mut self, request: &RecordedRequest) -> Option<FixtureResponse> {
        let target = request.line.split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("https://fixture.test{target}")).unwrap();
        let param = |key| {
            url.query_pairs()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.into_owned())
        };
        let header = |key| {
            request
                .headers
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        self.requests
            .push((target.to_owned(), header("harklight-ack2") == Some("true")));
        assert!(
            self.requests.len() < 32,
            "trace exceeded request bound: {:?}",
            self.requests
        );
        match url.path() {
            "/v2/agent/model-stop" => None,
            "/v2/agent/delivery-baseline" => {
                if self.unsupported {
                    return Some(baseline_unsupported());
                }
                let session = param("session");
                let Some(page) = param("page") else {
                    let attempt = self.tokens.len();
                    self.tokens.push(Token {
                        pin: self.head,
                        session,
                    });
                    let mut response = delivery_start(self.head, &format!("{attempt}-0"));
                    response.truncate = matches!(self.fault, Fault::PrepareResponse);
                    return Some(response);
                };
                let (attempt, offset) = page.split_once('-').unwrap();
                let attempt: usize = attempt.parse().unwrap();
                let offset: usize = offset.parse().unwrap();
                let token = &self.tokens[attempt];
                assert_eq!(token.session, session);
                let mut rows = Vec::new();
                let mut next = None;
                for (idx, turn) in self.turns.iter_mut().enumerate().skip(offset) {
                    if turn.row["rev"].as_u64().unwrap() > token.pin
                        || session
                            .as_ref()
                            .is_some_and(|id| turn.row["data"]["session"] != id.as_str())
                        || turn.served_by.is_some_and(|owner| owner != attempt)
                    {
                        continue;
                    }
                    if !rows.is_empty() {
                        next = Some(format!("{attempt}-{idx}"));
                        break;
                    }
                    turn.served_by = Some(attempt);
                    let mut row = turn.row.clone();
                    if row["kind"] == "entry" {
                        row["data"]["delivery"]["delivered"] = true.into();
                    } else {
                        row["data"]["terminal"]["delivery"]["delivered"] = true.into();
                    }
                    rows.push(row);
                }
                let mut response = delivery_page(token.pin, rows, next.as_deref());
                response.truncate = offset != 0 && matches!(self.fault, Fault::SecondPageResponse);
                Some(response)
            }
            "/v2/agent/changes" => {
                let sse = header("accept") == Some("text/event-stream");
                let after: u64 = param("after")
                    .or_else(|| header("last-event-id").map(str::to_owned))
                    .unwrap()
                    .parse()
                    .unwrap();
                if !(self.floor..=self.head).contains(&after) {
                    return Some(expired());
                }
                if sse && std::mem::take(&mut self.expire_on_stream) {
                    self.head += 10;
                    self.floor = self.head;
                    return Some(expired());
                }
                if sse {
                    // A terminal error lets Follow and empty Next traces leave the loop.
                    return Some(sse_closing("internal_error"));
                }
                let mut response = FixtureResponse::json(json!({
                    "rev": self.head, "changes": [], "more": false,
                }));
                response.truncate = matches!(self.fault, Fault::AfterCompletion);
                Some(response)
            }
            "/v2/agent/messages" => {
                let session = param("session");
                let messages: Vec<_> = self
                    .turns
                    .iter()
                    .filter(|turn| {
                        turn.row["kind"] == "message"
                            && session
                                .as_ref()
                                .is_none_or(|id| turn.row["data"]["session"] == id.as_str())
                    })
                    .map(|turn| turn.row["data"].clone())
                    .collect();
                Some(FixtureResponse::json(
                    json!({"rev": self.head, "messages": messages}),
                ))
            }
            _ => panic!("unexpected fixture route {target}"),
        }
    }
}

struct Harness {
    root: Scratch,
    transport: Option<SlowServer>,
    server: Arc<Mutex<Server>>,
    session: Uuid,
}

struct Call {
    rows: Vec<Value>,
    error: Option<FeedError>,
    requests: Vec<(String, bool)>,
    cursor: Option<u64>,
}

impl Call {
    fn turns(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| match row["kind"].as_str() {
                Some("entry") => row["data"]["entry_id"].as_str().map(str::to_owned),
                Some("message") => row["data"]["item_id"].as_str().map(str::to_owned),
                _ => None,
            })
            .collect()
    }
}

impl Harness {
    fn new() -> Self {
        let session = Uuid::from_u128(1);
        let server = Arc::new(Mutex::new(Server::new(session)));
        let state = Arc::clone(&server);
        let transport =
            SlowServer::respond_to(move |request| state.lock().unwrap().respond(request));
        let (root, _) = open();
        Self {
            root,
            transport: Some(transport),
            server,
            session,
        }
    }

    fn call(&self, start: Start, mode: Mode, ack2: bool, fault: Fault, session: Uuid) -> Call {
        let begin = {
            let mut server = self.server.lock().unwrap();
            server.fault = fault;
            server.requests.len()
        };
        // Each step reconstructs both the client and journal as a restarted
        // invocation would. Rows live only in this simulated receiver.
        let journal = JournalStore::open(&self.root.0, "fixture", Uuid::nil()).unwrap();
        let client = client(self.transport.as_ref().unwrap()).with_ack2(ack2);
        let mut rows = Vec::new();
        let error = run(&client, &journal, Some(session), start, mode, |row| {
            if matches!(
                (fault, row["kind"].as_str()),
                (Fault::OpeningRecord, Some("baseline"))
                    | (Fault::ChatRecord, Some("entry"))
                    | (Fault::CompletionRecord, Some("baseline_complete"))
                    | (Fault::CaughtUpRecord, Some("caught_up"))
            ) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            rows.push(row.clone());
            // The receiver got the complete line before the writer failed
            // its flush (or the process died before updating SQLite).
            if matches!(fault, Fault::CompletionFlush) && row["kind"] == "baseline_complete" {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            Ok(())
        })
        .err();
        let requests = self.server.lock().unwrap().requests[begin..].to_vec();
        let cursor = journal.v2_feed_cursor(&session.to_string()).unwrap();
        Call {
            rows,
            error,
            requests,
            cursor,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let transport = self.transport.take().unwrap();
        let _ = client(&transport).read(&["agent", "model-stop"], &[]);
        transport.requests();
    }
}

#[test]
fn caller_owned_starts_keep_legacy_feed_and_snapshot_paths() {
    for start in [
        Start::Snapshot,
        Start::After(3),
        Start::After(PIN),
        Start::After(FUTURE),
    ] {
        let model = Harness::new();
        // A partially emitted default baseline exists in the same journal.
        let default = model.call(
            Start::Journal,
            Mode::Read,
            true,
            Fault::CompletionRecord,
            model.session,
        );
        assert!(matches!(default.error, Some(FeedError::Output(_))));
        if !matches!(start, Start::After(PIN)) {
            let discarded = model.call(
                start,
                Mode::Read,
                true,
                Fault::CompletionRecord,
                model.session,
            );
            assert!(matches!(discarded.error, Some(FeedError::Output(_))));
            assert!(discarded.cursor.is_none());
            assert!(
                discarded
                    .requests
                    .iter()
                    .all(|(path, _)| !path.contains("delivery-baseline"))
            );
        }
        for (ack2, mode) in [(true, Mode::Read), (false, Mode::Follow)] {
            let call = model.call(start, mode, ack2, Fault::None, model.session);
            assert!(call.cursor.is_none());
            let requested: Vec<&str> = call
                .requests
                .iter()
                .map(|(path, _)| path.split('?').next().unwrap())
                .collect();
            let legacy_snapshot = !matches!(start, Start::After(PIN));
            let mut expected = Vec::new();
            if matches!(start, Start::After(_)) {
                expected.push("/v2/agent/changes");
            }
            if legacy_snapshot {
                expected.push("/v2/agent/messages");
                expected.push("/v2/agent/changes");
            }
            if !ack2 {
                expected.push("/v2/agent/changes");
                assert!(matches!(
                    call.error,
                    Some(FeedError::InternalClosing { .. })
                ));
            } else {
                assert!(call.error.is_none());
            }
            assert_eq!(requested, expected);
            assert_eq!(
                call.turns(),
                if legacy_snapshot {
                    vec!["answered"]
                } else {
                    vec![]
                }
            );
            for (path, declared) in call.requests {
                assert_eq!(declared, ack2 && path.starts_with("/v2/agent/changes"));
            }
        }
        let retry = model.call(
            Start::Journal,
            Mode::Next,
            false,
            Fault::None,
            model.session,
        );
        assert_eq!(retry.turns(), ["retained-chat", "answered"]);
        assert_eq!(retry.cursor, Some(PIN));
        assert!(retry.requests.iter().all(|(_, declared)| *declared));
    }
}

#[test]
fn baseline_model_default_faults_restart_and_commit_only_once() {
    for fault in [
        Fault::PrepareResponse,
        Fault::SecondPageResponse,
        Fault::OpeningRecord,
        Fault::ChatRecord,
        Fault::CompletionRecord,
        Fault::CompletionFlush,
        Fault::AfterCompletion,
        Fault::CaughtUpRecord,
    ] {
        let model = Harness::new();
        let first = model.call(Start::Journal, Mode::Read, true, fault, model.session);
        assert!(first.error.is_some(), "{fault:?}");
        let committed = matches!(fault, Fault::CaughtUpRecord);
        assert_eq!(first.cursor, committed.then_some(PIN), "{fault:?}");
        let second = model.call(
            Start::Journal,
            Mode::Next,
            false,
            Fault::None,
            model.session,
        );
        let expected: &[&str] = if committed {
            &[]
        } else {
            &["retained-chat", "answered"]
        };
        assert_eq!(second.turns(), expected, "{fault:?}");
        assert_eq!(second.cursor, Some(PIN));
        let third = model.call(
            Start::Journal,
            Mode::Read,
            false,
            Fault::None,
            model.session,
        );
        assert!(third.error.is_none());
        assert!(third.turns().is_empty());
    }
}

#[test]
fn baseline_model_filters_do_not_ack_another_scope() {
    let model = Harness::new();
    let first = model.call(
        Start::Journal,
        Mode::Read,
        true,
        Fault::CompletionRecord,
        model.session,
    );
    assert_eq!(first.turns(), ["retained-chat", "answered"]);
    let other = model.call(
        Start::Journal,
        Mode::Read,
        false,
        Fault::None,
        Uuid::new_v4(),
    );
    assert!(other.turns().is_empty());
    let retry = model.call(
        Start::Journal,
        Mode::Next,
        false,
        Fault::None,
        model.session,
    );
    assert_eq!(retry.turns(), ["retained-chat", "answered"]);
    assert!(retry.requests.iter().all(|(_, declared)| *declared));
}

#[test]
fn baseline_model_only_tokenless_unsupported_falls_back() {
    for pending in [false, true] {
        let model = Harness::new();
        if pending {
            let first = model.call(
                Start::Journal,
                Mode::Read,
                true,
                Fault::OpeningRecord,
                model.session,
            );
            assert!(matches!(first.error, Some(FeedError::Output(_))));
        }
        model.server.lock().unwrap().unsupported = true;
        let retry = model.call(
            Start::Journal,
            Mode::Read,
            false,
            Fault::None,
            model.session,
        );
        assert_eq!(retry.error.is_some(), pending);
        assert_eq!(
            retry
                .requests
                .iter()
                .any(|(path, _)| path.starts_with("/v2/agent/messages")),
            !pending
        );
    }
}

#[test]
fn baseline_model_follow_renews_after_sse_expiry_without_replaying_committed_work() {
    let model = Harness::new();
    let failed_capture = model.call(
        Start::Journal,
        Mode::Read,
        true,
        Fault::CompletionRecord,
        model.session,
    );
    assert!(matches!(failed_capture.error, Some(FeedError::Output(_))));
    model.server.lock().unwrap().expire_on_stream = true;
    let follow = model.call(
        Start::Journal,
        Mode::Follow,
        false,
        Fault::None,
        model.session,
    );
    assert!(matches!(
        follow.error,
        Some(FeedError::InternalClosing { .. })
    ));
    let last_baseline = follow
        .requests
        .iter()
        .rposition(|(path, _)| path.contains("page=0-0") || path.contains("page=0-1"))
        .unwrap();
    assert!(
        follow.requests[..=last_baseline]
            .iter()
            .all(|(_, declared)| *declared)
    );
    assert!(
        follow
            .requests
            .iter()
            .skip(last_baseline + 2)
            .all(|(_, declared)| !*declared)
    );
    assert!(
        follow
            .turns()
            .starts_with(&["retained-chat".into(), "answered".into()])
    );
    assert!(
        follow
            .rows
            .iter()
            .any(|row| row["kind"] == "caught_up" && row["rev"] == 50)
    );
    let resume = model.call(
        Start::Journal,
        Mode::Read,
        false,
        Fault::None,
        model.session,
    );
    assert!(resume.error.is_none());
    assert!(resume.turns().is_empty());
    assert!(resume.requests.iter().all(|(_, declared)| !*declared));
}
