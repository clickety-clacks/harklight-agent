use std::{
    collections::BTreeSet,
    io::{self, BufRead, BufReader},
    thread,
    time::Duration,
};

use reqwest::{
    blocking::Response,
    header::{ACCEPT, CONTENT_TYPE},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::journal::{JournalError, JournalStore};

use super::{AgentClient, V2Error, header};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Change {
    pub rev: u64,
    pub kind: String,
    pub data: Value,
}

impl Change {
    pub(crate) fn resource_key(&self) -> Result<String, FeedError> {
        let id = match self.kind.as_str() {
            "message" => self.data["item_id"].as_str(),
            "entry" => self.data["entry_id"].as_str(),
            "advisory" => self.data["advisory"]["advisory_id"].as_str(),
            _ => None,
        };
        let id = id
            .filter(|id| !id.is_empty())
            .ok_or(FeedError::InvalidChange)?;
        if self.rev == 0 || !self.data.is_object() {
            return Err(FeedError::InvalidChange);
        }
        if self.kind != "advisory" && self.data["rev"] != self.rev {
            return Err(FeedError::InvalidChange);
        }
        Ok(format!("{}:{id}", self.kind))
    }
}

#[derive(Deserialize)]
struct Batch {
    rev: u64,
    changes: Vec<Change>,
    more: bool,
}

pub enum Start {
    Journal,
    After(u64),
    Snapshot,
}

pub enum Mode {
    Read,
    Follow,
    Next,
    Focus,
}

#[derive(Debug, Error)]
pub enum FeedError {
    #[error(transparent)]
    Api(#[from] V2Error),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error("invalid agent change or cursor")]
    InvalidChange,
    #[error("invalid agent snapshot")]
    InvalidSnapshot,
    #[error("agent feed closed: {0}")]
    Closed(String),
    #[error("could not emit agent feed output: {0}")]
    Output(io::Error),
}

impl FeedError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Api(V2Error::Http {
                token: Some(token), ..
            }) if token == "unsupported" => 5,
            Self::Closed(_)
            | Self::Api(V2Error::Http {
                status: 400..=499, ..
            }) => 2,
            _ => 1,
        }
    }
}

impl AgentClient {
    fn change_batch(&self, session: Option<Uuid>, after: u64) -> Result<Batch, FeedError> {
        let after = after.to_string();
        let session = session.map(|session| session.to_string());
        let mut query = vec![("after", after.as_str())];
        if let Some(session) = &session {
            query.push(("session", session));
        }
        let value = self.read(&["agent", "changes"], &query)?;
        serde_json::from_value(value).map_err(|_| FeedError::InvalidChange)
    }

    fn open_changes(
        &self,
        stream_client: &reqwest::blocking::Client,
        session: Option<Uuid>,
        after: u64,
    ) -> Result<EventStream, FeedError> {
        let mut url = self.url(&["agent", "changes"])?;
        if let Some(session) = session {
            url.query_pairs_mut()
                .append_pair("session", &session.to_string());
        }
        let response = self.send_checked(
            stream_client
                .get(url)
                .header(ACCEPT, "text/event-stream")
                .header("Last-Event-ID", after.to_string()),
        )?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if content_type.split(';').next() != Some("text/event-stream") {
            return Err(V2Error::InvalidResponse {
                request_id: header(&response, "Request-Id").map(str::to_owned),
            }
            .into());
        }
        Ok(EventStream(BufReader::new(response)))
    }
}

struct EventStream(BufReader<Response>);

enum Event {
    Change(Change),
    Focus(Value),
    Heartbeat,
    Closing(String),
}

impl EventStream {
    fn next(&mut self) -> Result<Option<Event>, FeedError> {
        let mut event = String::new();
        let mut id = None;
        let mut data = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            match self.0.read_line(&mut line) {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) => {}
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if data.is_empty() {
                    event.clear();
                    id = None;
                    continue;
                }
                let data: Value =
                    serde_json::from_str(&data.join("\n")).map_err(|_| FeedError::InvalidChange)?;
                let parsed = match (event.as_str(), id) {
                    ("focus", None) if data["focus"].is_object() => Event::Focus(data),
                    ("heartbeat", None) => Event::Heartbeat,
                    ("closing", None) => Event::Closing(
                        data["reason"]
                            .as_str()
                            .ok_or(FeedError::InvalidChange)?
                            .to_owned(),
                    ),
                    ("message" | "entry" | "advisory", Some(rev)) => {
                        let change = Change {
                            rev,
                            kind: event,
                            data,
                        };
                        change.resource_key()?;
                        Event::Change(change)
                    }
                    _ => return Err(FeedError::InvalidChange),
                };
                return Ok(Some(parsed));
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event = value.to_owned(),
                "id" => id = Some(value.parse().map_err(|_| FeedError::InvalidChange)?),
                "data" => data.push(value.to_owned()),
                _ => {}
            }
        }
    }
}

pub fn run(
    client: &AgentClient,
    journal: &JournalStore,
    session: Option<Uuid>,
    start: Start,
    mode: Mode,
    mut output: impl FnMut(&Value) -> io::Result<()>,
) -> Result<(), FeedError> {
    // The blocking transport applies this timeout to each response read.
    // Healthy feeds carry a heartbeat well within it.
    let stream_client = if matches!(mode, Mode::Read) {
        None
    } else {
        Some(
            client
                .identity
                .device_client_with_timeout(Some(Duration::from_secs(60)))?,
        )
    };
    let scope = session.map(|id| id.to_string()).unwrap_or_default();
    let uses_journal = matches!(start, Start::Journal) && !matches!(mode, Mode::Focus);
    let saved = if uses_journal {
        journal.v2_feed_cursor(&scope)?
    } else {
        None
    };
    let mut cursor = match start {
        Start::After(rev) => Some(rev),
        _ => saved,
    };
    let mut baseline_reason = "first_contact";
    let mut checkpoint = saved;
    let mut backoff_secs = 1u64;
    let focus_only = matches!(mode, Mode::Focus);
    if focus_only {
        let focus = client.read(&["agent", "focus"], &[])?;
        output(&json!({"kind":"focus","data":{"focus":focus}})).map_err(FeedError::Output)?;
        cursor = Some(0);
    }

    'resume: loop {
        let mut start_position = match cursor {
            Some(rev) => rev,
            None => match baseline(client, session) {
                Ok((rev, records)) => {
                    emit_value(
                        &mut output,
                        json!({"kind":"baseline","rev":rev,"reason":baseline_reason}),
                    )?;
                    for change in &records {
                        emit_change(&mut output, change)?;
                    }
                    emit_value(&mut output, json!({"kind":"baseline_complete","rev":rev}))?;
                    if uses_journal {
                        journal.record_v2_feed(&scope, checkpoint, rev, &records, true)?;
                        checkpoint = Some(rev);
                    }
                    rev
                }
                Err(error) if retry_delay(&error).is_some() && !matches!(mode, Mode::Read) => {
                    delay(&mut backoff_secs, retry_delay(&error));
                    continue;
                }
                Err(error) => return Err(error),
            },
        };

        let mut output_changes = false;
        loop {
            let batch = match client.change_batch(session, start_position) {
                Ok(batch) => batch,
                Err(error) if cursor_expired(&error) => {
                    cursor = None;
                    baseline_reason = "cursor_expired";
                    if focus_only {
                        cursor = Some(first_snapshot_position(client, session)?);
                    }
                    continue 'resume;
                }
                Err(error) if retry_delay(&error).is_some() && !matches!(mode, Mode::Read) => {
                    cursor = Some(start_position);
                    delay(&mut backoff_secs, retry_delay(&error));
                    continue 'resume;
                }
                Err(error) => return Err(error),
            };
            let mut previous = start_position;
            for change in &batch.changes {
                change.resource_key()?;
                if change.rev <= previous || change.rev > batch.rev {
                    return Err(FeedError::InvalidChange);
                }
                previous = change.rev;
            }
            if batch.rev < start_position || (batch.more && batch.rev == start_position) {
                return Err(FeedError::InvalidChange);
            }
            if !focus_only {
                for change in &batch.changes {
                    emit_change(&mut output, change)?;
                }
            }
            if uses_journal {
                journal.record_v2_feed(&scope, checkpoint, batch.rev, &batch.changes, false)?;
                checkpoint = Some(batch.rev);
            }
            output_changes |= !batch.changes.is_empty();
            start_position = batch.rev;
            if !batch.more {
                break;
            }
        }
        if !focus_only {
            emit_value(
                &mut output,
                json!({"kind":"caught_up","rev":start_position}),
            )?;
        }
        if matches!(mode, Mode::Read) || (matches!(mode, Mode::Next) && output_changes) {
            return Ok(());
        }
        let Some(stream_client) = &stream_client else {
            return Ok(());
        };
        let mut stream = match client.open_changes(stream_client, session, start_position) {
            Ok(stream) => stream,
            Err(error) if cursor_expired(&error) => {
                cursor = if focus_only {
                    Some(first_snapshot_position(client, session)?)
                } else {
                    None
                };
                baseline_reason = "cursor_expired";
                continue;
            }
            Err(error) if retry_delay(&error).is_some() => {
                cursor = Some(start_position);
                delay(&mut backoff_secs, retry_delay(&error));
                continue;
            }
            Err(error) => return Err(error),
        };
        while let Some(event) = stream.next()? {
            match event {
                Event::Change(change) => {
                    if change.rev <= start_position {
                        continue;
                    }
                    if !focus_only {
                        emit_change(&mut output, &change)?;
                    }
                    if uses_journal {
                        journal.record_v2_feed(
                            &scope,
                            checkpoint,
                            change.rev,
                            std::slice::from_ref(&change),
                            false,
                        )?;
                        checkpoint = Some(change.rev);
                    }
                    start_position = change.rev;
                    if matches!(mode, Mode::Next) {
                        return Ok(());
                    }
                }
                Event::Focus(data) => emit_value(&mut output, json!({"kind":"focus","data":data}))?,
                Event::Heartbeat => {}
                Event::Closing(reason) => match reason.as_str() {
                    "cursor_expired" => {
                        cursor = if focus_only {
                            Some(first_snapshot_position(client, session)?)
                        } else {
                            None
                        };
                        baseline_reason = "cursor_expired";
                        continue 'resume;
                    }
                    "storage_unavailable" | "server_shutdown" => break,
                    _ => return Err(FeedError::Closed(reason)),
                },
            }
            backoff_secs = 1;
        }
        cursor = Some(start_position);
        delay(&mut backoff_secs, None);
    }
}

fn emit_value(
    output: &mut impl FnMut(&Value) -> io::Result<()>,
    value: Value,
) -> Result<(), FeedError> {
    output(&value).map_err(FeedError::Output)
}

fn emit_change(
    output: &mut impl FnMut(&Value) -> io::Result<()>,
    change: &Change,
) -> Result<(), FeedError> {
    emit_value(
        output,
        json!({"kind":change.kind,"rev":change.rev,"data":change.data}),
    )
}

fn retry_delay(error: &FeedError) -> Option<u64> {
    match error {
        FeedError::Api(V2Error::Http {
            status: 503,
            retry_after,
            ..
        }) => Some(retry_after.unwrap_or(0)),
        FeedError::Api(V2Error::Transport { .. } | V2Error::ResponseRead { .. }) => Some(0),
        _ => None,
    }
}

fn delay(seconds: &mut u64, retry_after: Option<u64>) {
    thread::sleep(Duration::from_secs(
        (*seconds).max(retry_after.unwrap_or(0)),
    ));
    *seconds = (*seconds * 2).min(25);
}

fn cursor_expired(error: &FeedError) -> bool {
    matches!(error, FeedError::Api(V2Error::Http { status: 410, token: Some(token), .. }) if token == "cursor_expired")
}

fn first_snapshot_position(client: &AgentClient, session: Option<Uuid>) -> Result<u64, FeedError> {
    client.messages(session, None)?["rev"]
        .as_u64()
        .ok_or(FeedError::InvalidSnapshot)
}

fn baseline(client: &AgentClient, session: Option<Uuid>) -> Result<(u64, Vec<Change>), FeedError> {
    let mut records = Vec::new();
    let mut first_position = None;
    let mut token = None;
    let mut tokens = BTreeSet::new();
    loop {
        let page = client.messages(session, token.as_deref())?;
        let position = page["rev"].as_u64().ok_or(FeedError::InvalidSnapshot)?;
        first_position.get_or_insert(position);
        let messages = page["messages"]
            .as_array()
            .ok_or(FeedError::InvalidSnapshot)?;
        for message in messages {
            let rev = message["rev"].as_u64().ok_or(FeedError::InvalidSnapshot)?;
            let item_id = message["item_id"]
                .as_str()
                .ok_or(FeedError::InvalidSnapshot)?;
            if session.is_some_and(|session| message["session"] != session.to_string()) {
                return Err(FeedError::InvalidSnapshot);
            }
            add_snapshot(&mut records, "message", rev, message.clone())?;
            if !message["terminal"].is_null() {
                continue;
            }
            if let Some(advisories) = message["advisories"].as_array() {
                for advisory in advisories {
                    add_snapshot(
                        &mut records,
                        "advisory",
                        rev,
                        json!({
                            "item_id":item_id, "advisory":advisory,
                        }),
                    )?;
                }
            }
            if !message["session"].is_null() {
                snapshot_entries(client, item_id, &mut records)?;
            }
        }
        match page["next_page"].as_str() {
            Some(next) if !next.is_empty() && tokens.insert(next.to_owned()) => {
                token = Some(next.to_owned());
            }
            None if page["next_page"].is_null() => break,
            _ => return Err(FeedError::InvalidSnapshot),
        }
    }
    Ok((first_position.ok_or(FeedError::InvalidSnapshot)?, records))
}

fn snapshot_entries(
    client: &AgentClient,
    item_id: &str,
    records: &mut Vec<Change>,
) -> Result<(), FeedError> {
    let mut before = None;
    let mut visited = BTreeSet::new();
    loop {
        let page = client.message_entries(item_id, before.as_deref(), None)?;
        let entries = page["entries"]
            .as_array()
            .ok_or(FeedError::InvalidSnapshot)?;
        for entry in entries {
            if entry["item_id"] != item_id {
                return Err(FeedError::InvalidSnapshot);
            }
            let rev = entry["rev"].as_u64().ok_or(FeedError::InvalidSnapshot)?;
            add_snapshot(records, "entry", rev, entry.clone())?;
        }
        match page["older"].as_bool() {
            Some(false) => return Ok(()),
            Some(true) => {
                let first = entries
                    .first()
                    .and_then(|entry| entry["entry_id"].as_str())
                    .filter(|id| visited.insert((*id).to_owned()))
                    .ok_or(FeedError::InvalidSnapshot)?;
                before = Some(first.to_owned());
            }
            None => return Err(FeedError::InvalidSnapshot),
        }
    }
}

fn add_snapshot(
    records: &mut Vec<Change>,
    kind: &str,
    rev: u64,
    data: Value,
) -> Result<(), FeedError> {
    let change = Change {
        kind: kind.to_owned(),
        rev,
        data,
    };
    change.resource_key()?;
    records.push(change);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{
        config::EnrolledIdentity,
        test_tls::{FixtureResponse, SlowServer, TLS_NAME},
    };

    use super::*;

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open() -> (Scratch, JournalStore) {
        let root =
            Scratch(std::env::temp_dir().join(format!("agent-feed-test-{}", Uuid::new_v4())));
        let journal = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        (root, journal)
    }

    fn client(server: &SlowServer) -> AgentClient {
        AgentClient::new(EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        ))
        .unwrap()
    }

    #[test]
    fn session_feed_resumes_from_last_event_id_and_keeps_sparse_revs() {
        let (_root, journal) = open();
        let session = Uuid::new_v4();
        let server = SlowServer::sequence(vec![
            FixtureResponse::json(json!({"rev":12,"changes":[],"more":false})),
            FixtureResponse {
                status: "200 OK", headers: &[("Content-Type","text/event-stream")],
                body: concat!(
                    "event: heartbeat\r\ndata: {}\r\n\r\n",
                    "event: focus\ndata: {\"focus\":{\"clients\":[]}}\n\n",
                    "id: 17\nevent: entry\ndata: {\"entry_id\":\"opaque\",\"item_id\":\"item\",\"rev\":17}\n\n",
                ).into(),
                truncate: false,
            },
        ]);
        let mut output = Vec::new();
        run(
            &client(&server),
            &journal,
            Some(session),
            Start::After(7),
            Mode::Next,
            |value| {
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap();
        let requests = server.requests();
        assert_eq!(
            requests[0].line.trim_end(),
            format!("GET /v2/agent/changes?after=7&session={session} HTTP/1.1")
        );
        assert_eq!(
            requests[1].line.trim_end(),
            format!("GET /v2/agent/changes?session={session} HTTP/1.1")
        );
        assert!(
            requests[1]
                .headers
                .contains(&("last-event-id".into(), "12".into()))
        );
        assert_eq!(output[0], json!({"kind":"caught_up","rev":12}));
        assert_eq!(output[1]["kind"], "focus");
        assert!(output[1].get("rev").is_none());
        assert_eq!(output[2]["rev"], 17);
        assert_eq!(journal.v2_feed_cursor(&session.to_string()).unwrap(), None);
    }

    fn expired() -> FixtureResponse {
        FixtureResponse {
            status: "410 Gone",
            headers: &[],
            body: r#"{"error":"cursor_expired","retryable":false,"request_id":"expired"}"#.into(),
            truncate: false,
        }
    }

    fn rebuild(session: Uuid) -> Vec<FixtureResponse> {
        vec![
            expired(),
            FixtureResponse::json(json!({"rev":40,"messages":[{
                "item_id":"item","session":session,"rev":39,"sender":"app:fixture",
                "revision":1,"message":{},"seen":false,"files":[],
                "advisories":[{"advisory_id":"notice","advisory_type":"possible_contradictions"}]
            }], "next_page":"later"})),
            FixtureResponse::json(json!({"rev":41,"entries":[{
                "entry_id":"entry-b","item_id":"item","rev":38,"position":2
            }],"older":true})),
            FixtureResponse::json(json!({"rev":43,"entries":[{
                "entry_id":"entry-a","item_id":"item","rev":37,"position":1
            }],"older":false})),
            FixtureResponse::json(json!({"rev":47,"messages":[{
                "item_id":"answered","session":session,"rev":42,"sender":"app:fixture",
                "revision":1,"message":{},"seen":false,"files":[],"terminal":{
                    "state":"answered","at_ms":100,"answer":null
                }
            }]})),
        ]
    }

    #[test]
    fn caller_cursor_rebuilds_again_after_a_partial_baseline_and_shared_reader_advance() {
        let (_root, journal) = open();
        let session = Uuid::new_v4();
        let scope = session.to_string();
        let partial = SlowServer::sequence(rebuild(session));
        let mut output = Vec::new();
        let error = run(
            &client(&partial),
            &journal,
            Some(session),
            Start::After(2),
            Mode::Read,
            |value| {
                if value["kind"] == "entry" {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(error, FeedError::Output(_)));
        assert_eq!(
            output[0],
            json!({"kind":"baseline","reason":"cursor_expired","rev":40})
        );
        assert!(
            output
                .iter()
                .all(|value| value["kind"] != "baseline_complete")
        );
        partial.requests();
        journal
            .record_v2_feed(&scope, None, 50, &[], false)
            .unwrap();

        let mut replies = rebuild(session);
        replies.push(FixtureResponse::json(
            json!({"rev":47,"changes":[],"more":false}),
        ));
        let restarted = SlowServer::sequence(replies);
        output.clear();
        run(
            &client(&restarted),
            &journal,
            Some(session),
            Start::After(2),
            Mode::Read,
            |value| {
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap();
        let requests = restarted.requests();
        assert!(requests[0].line.contains("after=2"));
        assert!(requests[3].line.contains("before=entry-b"));
        assert!(requests[4].line.contains("page=later"));
        assert!(requests[5].line.contains("after=40"));
        assert_eq!(
            output
                .iter()
                .filter(|value| value["kind"] == "entry")
                .count(),
            2
        );
        assert_eq!(
            output
                .iter()
                .filter(|value| value["kind"] == "advisory")
                .count(),
            1
        );
        assert!(
            output
                .iter()
                .any(|value| value["data"]["terminal"]["answer"].is_null()
                    && value["data"]["terminal"]["state"] == "answered")
        );
        assert_eq!(
            output[output.len() - 2],
            json!({"kind":"baseline_complete","rev":40})
        );
        assert_eq!(output.last(), Some(&json!({"kind":"caught_up","rev":47})));
        assert_eq!(journal.v2_feed_cursor(&scope).unwrap(), Some(50));
    }

    #[test]
    fn default_checkpoint_is_per_scope_and_advances_through_an_empty_batch() {
        let (_root, journal) = open();
        let session = Uuid::new_v4();
        let server = SlowServer::sequence(vec![
            FixtureResponse::json(json!({"rev":2,"messages":[]})),
            FixtureResponse::json(json!({"rev":7,"changes":[],"more":false})),
        ]);
        let mut output = Vec::new();
        run(
            &client(&server),
            &journal,
            Some(session),
            Start::Journal,
            Mode::Read,
            |value| {
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            journal.v2_feed_cursor(&session.to_string()).unwrap(),
            Some(7)
        );
        assert_eq!(journal.v2_feed_cursor("").unwrap(), None);
        assert_eq!(output.last(), Some(&json!({"kind":"caught_up","rev":7})));
        assert!(server.requests()[1].line.contains("after=2"));
    }
    #[test]
    fn default_checkpoint_does_not_advance_when_output_is_interrupted() {
        let (_root, journal) = open();
        let baseline =
            SlowServer::sequence(vec![FixtureResponse::json(json!({"rev":40,"messages":[]}))]);
        let error = run(
            &client(&baseline),
            &journal,
            None,
            Start::Journal,
            Mode::Read,
            |record| {
                assert_eq!(journal.v2_feed_cursor("").unwrap(), None);
                if record["kind"] == "baseline_complete" {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(error, FeedError::Output(_)));
        assert_eq!(journal.v2_feed_cursor("").unwrap(), None);
        baseline.requests();

        journal.record_v2_feed("", None, 40, &[], false).unwrap();
        let live = SlowServer::sequence(vec![
            FixtureResponse::json(json!({"rev":40,"changes":[],"more":false})),
            FixtureResponse {
                status: "200 OK",
                headers: &[("Content-Type", "text/event-stream")],
                body: concat!(
                    "id: 41\n",
                    "event: entry\n",
                    r#"data: {"entry_id":"entry","item_id":"item","rev":41}"#,
                    "\n\n"
                )
                .into(),
                truncate: false,
            },
        ]);
        let error = run(
            &client(&live),
            &journal,
            None,
            Start::Journal,
            Mode::Next,
            |record| {
                if record["kind"] == "entry" {
                    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(40));
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(error, FeedError::Output(_)));
        assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(40));
        live.requests();
    }
}
