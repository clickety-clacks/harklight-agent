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

mod delivery;
use delivery::{baseline_replay, delivery_baseline};

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

#[derive(Clone, Copy)]
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
    #[error("internal_error (Request-Id {request_id:?})")]
    InternalClosing { request_id: Option<String> },
    #[error("could not emit agent feed output: {0}")]
    Output(io::Error),
}

impl FeedError {
    pub fn is_internal_error(&self) -> bool {
        match self {
            Self::InternalClosing { .. } => true,
            Self::Api(error) => error.is_internal_error(),
            _ => false,
        }
    }

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
    pub fn with_ack2(mut self, ack2: bool) -> Self {
        self.ack2 = ack2;
        self
    }

    fn delivery_request(
        request: reqwest::blocking::RequestBuilder,
        ack2: bool,
    ) -> reqwest::blocking::RequestBuilder {
        if ack2 {
            request.header("Harklight-Ack2", "true")
        } else {
            request
        }
    }

    fn change_batch(&self, session: Option<Uuid>, after: u64) -> Result<Batch, FeedError> {
        let mut url = self.url(&["agent", "changes"])?;
        url.query_pairs_mut()
            .append_pair("after", &after.to_string());
        if let Some(session) = session {
            url.query_pairs_mut()
                .append_pair("session", &session.to_string());
        }
        let request = self.client.get(url).header(ACCEPT, "application/json");
        let value = self.read_json(Self::delivery_request(request, self.ack2))?;
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
        let request = stream_client
            .get(url)
            .header(ACCEPT, "text/event-stream")
            .header("Last-Event-ID", after.to_string());
        let response = self.send_checked(Self::delivery_request(request, self.ack2))?;
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
        Ok(EventStream {
            request_id: header(&response, "Request-Id").map(str::to_owned),
            reader: BufReader::new(response),
        })
    }

    fn delivery_page(
        &self,
        session: Option<Uuid>,
        page: Option<&str>,
        ack2: bool,
    ) -> Result<Value, FeedError> {
        let mut url = self.url(&["agent", "delivery-baseline"])?;
        if let Some(session) = session {
            url.query_pairs_mut()
                .append_pair("session", &session.to_string());
        }
        if let Some(page) = page {
            url.query_pairs_mut().append_pair("page", page);
        }
        let request = self.client.get(url).header(ACCEPT, "application/json");
        Ok(self.read_json(Self::delivery_request(request, ack2))?)
    }
}

struct EventStream {
    reader: BufReader<Response>,
    request_id: Option<String>,
}

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
            match self.reader.read_line(&mut line) {
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
    let reader = match start {
        Start::Journal => "journal",
        Start::After(_) | Start::Snapshot => "caller",
    };
    let baseline_key = format!(
        "delivery-baseline:{}:{scope}:{reader}",
        client.server_binding()
    );
    let mut checkpoint = saved;
    let mut backoff_secs = 1u64;
    let focus_only = matches!(mode, Mode::Focus);
    let caller_supplies_cursor = matches!(start, Start::After(_) | Start::Snapshot);
    if !focus_only && journal.delivery_attempt(&baseline_key)?.is_some() {
        let _lock = journal.lock_delivery_baseline(&baseline_key)?;
        if let Some(mut attempt) = journal.delivery_attempt(&baseline_key)? {
            if matches!(start, Start::After(after) if attempt.complete && after >= attempt.rev) {
                journal.clear_delivery_attempt(&baseline_key)?;
            } else {
                if let Start::After(after) = start {
                    let retained = attempt.prior.len();
                    attempt.prior.retain(|pages| pages.rev > after);
                    if attempt.prior.len() != retained {
                        journal.save_delivery_attempt(&baseline_key, &attempt)?;
                    }
                }
                cursor = None;
            }
        }
    }
    if focus_only {
        let focus = client.read(&["agent", "focus"], &[])?;
        output(&json!({"kind":"focus","data":{"focus":focus}})).map_err(FeedError::Output)?;
        cursor = Some(0);
    }

    let mut output_changes = false;
    'resume: loop {
        let mut delivery_lock = None;
        let mut start_position = match cursor {
            Some(rev) => rev,
            None => {
                delivery_lock = Some(journal.lock_delivery_baseline(&baseline_key)?);
                if uses_journal {
                    let current = journal.v2_feed_cursor(&scope)?;
                    if current != checkpoint {
                        checkpoint = current;
                        cursor = current;
                        continue 'resume;
                    }
                }
                match delivery_baseline(
                    client,
                    journal,
                    session,
                    &baseline_key,
                    baseline_reason,
                    caller_supplies_cursor,
                    &mut output,
                ) {
                    Ok(Some((rev, emitted))) => {
                        output_changes |= emitted;
                        rev
                    }
                    Ok(None) => {
                        drop(delivery_lock.take());
                        match baseline(client, session) {
                            Ok((rev, records)) => {
                                emit_value(
                                    &mut output,
                                    json!({"kind":"baseline","rev":rev,"reason":baseline_reason}),
                                )?;
                                for change in &records {
                                    emit_change(&mut output, change)?;
                                }
                                emit_value(
                                    &mut output,
                                    json!({"kind":"baseline_complete","rev":rev}),
                                )?;
                                if uses_journal {
                                    journal
                                        .record_v2_feed(&scope, checkpoint, rev, &records, true)?;
                                    checkpoint = Some(rev);
                                }
                                rev
                            }
                            Err(error)
                                if retry_delay(&error).is_some() && !matches!(mode, Mode::Read) =>
                            {
                                delay(&mut backoff_secs, retry_delay(&error));
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Err(error) if retry_delay(&error).is_some() && !matches!(mode, Mode::Read) => {
                        drop(delivery_lock);
                        delay(&mut backoff_secs, retry_delay(&error));
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
        };

        let mut applying_delivery = delivery_lock.is_some();
        loop {
            let batch = match client.change_batch(session, start_position) {
                Ok(batch) => batch,
                Err(error) if cursor_expired(&error) => {
                    if applying_delivery {
                        if caller_supplies_cursor {
                            let mut attempt = journal
                                .delivery_attempt(&baseline_key)?
                                .ok_or(FeedError::InvalidSnapshot)?;
                            attempt.renew = true;
                            journal.save_delivery_attempt(&baseline_key, &attempt)?;
                        } else {
                            journal.clear_delivery_attempt(&baseline_key)?;
                        }
                    }
                    cursor = None;
                    baseline_reason = "cursor_expired";
                    if focus_only {
                        cursor = Some(first_snapshot_position(client, session)?);
                    }
                    continue 'resume;
                }
                Err(error) if retry_delay(&error).is_some() && !matches!(mode, Mode::Read) => {
                    cursor = (!applying_delivery).then_some(start_position);
                    drop(delivery_lock.take());
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
            if applying_delivery {
                if !caller_supplies_cursor {
                    journal.finish_delivery_baseline(
                        &baseline_key,
                        uses_journal.then_some(scope.as_str()),
                        checkpoint,
                        start_position,
                    )?;
                }
                if uses_journal {
                    checkpoint = Some(start_position);
                }
                applying_delivery = false;
                drop(delivery_lock.take());
            }
            if !focus_only {
                for change in &batch.changes {
                    if !baseline_replay(journal, &baseline_key, change)? {
                        emit_change(&mut output, change)?;
                        output_changes = true;
                    }
                }
            }
            if uses_journal {
                journal.record_feed_after_delivery(
                    &scope,
                    checkpoint,
                    batch.rev,
                    &batch.changes,
                    &baseline_key,
                )?;
                checkpoint = Some(batch.rev);
            }
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
                    let emitted = !focus_only && !baseline_replay(journal, &baseline_key, &change)?;
                    if emitted {
                        emit_change(&mut output, &change)?;
                    }
                    if uses_journal {
                        journal.record_feed_after_delivery(
                            &scope,
                            checkpoint,
                            change.rev,
                            std::slice::from_ref(&change),
                            &baseline_key,
                        )?;
                        checkpoint = Some(change.rev);
                    }
                    start_position = change.rev;
                    if emitted && matches!(mode, Mode::Next) {
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
                    "internal_error" => {
                        return Err(FeedError::InternalClosing {
                            request_id: stream.request_id,
                        });
                    }
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

    fn baseline_unsupported() -> FixtureResponse {
        FixtureResponse {
            status: "404 Not Found",
            headers: &[],
            body: r#"{"error":"not_found","retryable":false,"request_id":"unsupported"}"#.into(),
            truncate: false,
        }
    }

    fn retained_turns(session: Uuid) -> (Value, Value) {
        let chat = json!({
            "rev":36,"kind":"entry","data":{
                "entry_id":"retained-chat","item_id":"answered","session":session,
                "rev":36,"position":1,"kind":"reply","author":"person",
                "request_revision":1,"text":"Waiting chat","delivery":{"delivered":false}
            }
        });
        let answer = json!({
            "rev":38,"kind":"message","data":{
                "item_id":"answered","session":session,"rev":38,"sender":"app:fixture",
                "legacy_message_id":"old-request","revision":1,"message":{},"seen":false,"files":[],
                "terminal":{"state":"answered","at_ms":100,"answer":null,
                    "delivery":{"delivered":false}}
            }
        });
        (chat, answer)
    }

    fn delivery_page(rev: u64, changes: Vec<Value>, next: Option<&str>) -> FixtureResponse {
        let mut page = json!({"rev":rev,"changes":changes});
        if let Some(next) = next {
            page["next_page"] = next.into();
        }
        FixtureResponse::json(page)
    }

    fn delivery_start(rev: u64, page: &str) -> FixtureResponse {
        FixtureResponse::json(json!({"rev":rev,"next_page":page}))
    }

    #[test]
    fn first_delivery_read_keeps_retained_chat_and_answer_and_skips_ack1_only_replay() {
        let (_root, journal) = open();
        let session = Uuid::new_v4();
        let scope = session.to_string();
        let (chat, mut answer) = retained_turns(session);
        let mut stale_answer = answer.clone();
        stale_answer["rev"] = 42.into();
        stale_answer["data"]["rev"] = 42.into();
        stale_answer["data"]["seen"] = true.into();
        answer["rev"] = 45.into();
        answer["data"]["rev"] = 45.into();
        let mut ack1 = chat.clone();
        ack1["rev"] = 41.into();
        ack1["data"]["rev"] = 41.into();
        ack1["data"]["delivery"]["delivered"] = true.into();
        let mut edited_answer = answer.clone();
        edited_answer["rev"] = 46.into();
        edited_answer["data"]["rev"] = 46.into();
        edited_answer["data"]["result"] = json!({"status":"applied"});
        let mut arriving_chat = chat.clone();
        arriving_chat["rev"] = 47.into();
        arriving_chat["data"]["rev"] = 47.into();
        arriving_chat["data"]["entry_id"] = "arrived-after-bound".into();
        let server = SlowServer::sequence(vec![
            delivery_start(40, "first"),
            delivery_page(40, vec![chat.clone()], Some("second")),
            delivery_page(40, vec![answer.clone()], None),
            FixtureResponse::json(
                json!({"rev":47,"changes":[ack1,stale_answer,edited_answer.clone(),arriving_chat.clone()],"more":false}),
            ),
            FixtureResponse::json(json!({"rev":47,"changes":[],"more":false})),
        ]);
        let client = client(&server);
        let mut output = Vec::new();
        run(
            &client,
            &journal,
            Some(session),
            Start::Journal,
            Mode::Read,
            |value| {
                if output.len() < 4 {
                    assert_eq!(journal.v2_feed_cursor(&scope).unwrap(), None);
                }
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            output,
            [
                json!({"kind":"baseline","reason":"first_contact","rev":40}),
                chat,
                answer,
                json!({"kind":"baseline_complete","rev":40}),
                edited_answer,
                arriving_chat,
                json!({"kind":"caught_up","rev":47}),
            ]
        );
        output.clear();
        run(
            &client,
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
        assert_eq!(output, [json!({"kind":"caught_up","rev":47})]);
        assert_eq!(journal.v2_feed_cursor(&scope).unwrap(), Some(47));
        assert_eq!(journal.v2_feed_cursor("").unwrap(), None);
        let requests = server.requests();
        assert!(
            requests[0]
                .line
                .contains(&format!("/agent/delivery-baseline?session={session} "))
        );
        assert!(requests[1].line.contains("&page=first "));
        assert!(requests[2].line.contains("&page=second "));
        assert!(requests[3].line.contains("after=40"));
        assert!(requests[4].line.contains("after=47"));
    }

    #[test]
    fn an_active_delivery_attempt_never_falls_back_to_the_legacy_snapshot() {
        let (_root, journal) = open();
        let server = SlowServer::sequence(vec![
            delivery_start(40, "active"),
            baseline_unsupported(),
            delivery_page(40, vec![], None),
            FixtureResponse::json(json!({"rev":40,"changes":[],"more":false})),
        ]);
        let client = client(&server).with_ack2(true);
        let error = run(&client, &journal, None, Start::Journal, Mode::Read, |_| {
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(
            error,
            FeedError::Api(V2Error::Http { status: 404, .. })
        ));
        assert_eq!(journal.v2_feed_cursor("").unwrap(), None);
        let client = client.with_ack2(false);
        run(&client, &journal, None, Start::Journal, Mode::Read, |_| {
            Ok(())
        })
        .unwrap();
        let requests = server.requests();
        assert_eq!(requests[1].line, requests[2].line);
        assert!(
            requests[2]
                .headers
                .contains(&("harklight-ack2".into(), "true".into()))
        );
        assert!(
            !requests[3]
                .headers
                .iter()
                .any(|(name, _)| name == "harklight-ack2")
        );
    }

    #[test]
    fn caller_checkpoint_can_resume_the_same_baseline_after_an_uncommitted_read() {
        let (_root, journal) = open();
        let session = Uuid::new_v4();
        let (chat, _) = retained_turns(session);
        let server = SlowServer::sequence(vec![
            delivery_start(40, "same-attempt"),
            delivery_page(40, vec![chat.clone()], None),
            FixtureResponse::json(json!({"rev":40,"changes":[],"more":false})),
            delivery_page(40, vec![chat.clone()], None),
            FixtureResponse::json(json!({"rev":40,"changes":[],"more":false})),
            FixtureResponse::json(json!({"rev":41,"changes":[],"more":false})),
        ]);
        let client = client(&server);
        for start in [Start::Snapshot, Start::Snapshot, Start::After(40)] {
            let mut output = Vec::new();
            run(
                &client,
                &journal,
                Some(session),
                start,
                Mode::Read,
                |value| {
                    output.push(value.clone());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(output.contains(&chat), matches!(start, Start::Snapshot));
        }
        assert_eq!(journal.v2_feed_cursor(&session.to_string()).unwrap(), None);
        let requests = server.requests();
        assert_eq!(requests[1].line, requests[3].line);
        assert!(requests[5].line.contains("after=40"));
    }

    #[test]
    fn caller_recovery_retains_an_unacknowledged_attempt_across_baseline_expiry() {
        let (root, journal) = open();
        let session = Uuid::new_v4();
        let (chat, answer) = retained_turns(session);
        let server = SlowServer::sequence(vec![
            delivery_start(40, "old"),
            delivery_page(40, vec![chat.clone()], None),
            expired(),
            delivery_start(50, "new"),
            delivery_page(40, vec![chat.clone()], None),
            baseline_unsupported(),
            delivery_page(40, vec![chat.clone()], None),
            delivery_page(50, vec![answer.clone()], None),
            FixtureResponse::json(json!({"rev":50,"changes":[],"more":false})),
            FixtureResponse::json(json!({"rev":51,"changes":[],"more":false})),
        ]);
        let client = client(&server);
        let error = run(
            &client,
            &journal,
            Some(session),
            Start::Snapshot,
            Mode::Read,
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            FeedError::Api(V2Error::Http { status: 404, .. })
        ));

        drop(journal);
        let journal = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        let mut output = Vec::new();
        run(
            &client,
            &journal,
            Some(session),
            Start::Snapshot,
            Mode::Read,
            |value| {
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            output,
            [
                json!({"kind":"baseline","reason":"cursor_expired","rev":50}),
                chat,
                answer,
                json!({"kind":"baseline_complete","rev":50}),
                json!({"kind":"caught_up","rev":50}),
            ]
        );
        run(
            &client,
            &journal,
            Some(session),
            Start::After(50),
            Mode::Read,
            |value| {
                assert_eq!(*value, json!({"kind":"caught_up","rev":51}));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(journal.v2_feed_cursor(&session.to_string()).unwrap(), None);
        let requests = server.requests();
        assert_eq!(requests[1].line, requests[4].line);
        assert_eq!(requests[4].line, requests[6].line);
        assert_eq!(requests[5].line, requests[7].line);
        assert!(requests[9].line.contains("after=50"));
    }

    #[test]
    fn interrupted_expired_cursor_baseline_resumes_after_restart_and_before_listening() {
        let (root, journal) = open();
        let session = Uuid::new_v4();
        let scope = session.to_string();
        let (chat, answer) = retained_turns(session);
        journal.record_v2_feed(&scope, None, 3, &[], false).unwrap();
        let server = SlowServer::sequence(vec![
            expired(),
            delivery_start(40, "first"),
            delivery_page(40, vec![chat.clone()], Some("second")),
            delivery_page(40, vec![answer.clone()], None),
            delivery_page(40, vec![chat.clone()], Some("second")),
            delivery_page(40, vec![answer.clone()], None),
            expired(),
            delivery_start(50, "later"),
            delivery_page(50, vec![], None),
            FixtureResponse::json(json!({"rev":50,"changes":[],"more":false})),
        ]);
        let client = client(&server);
        let err = run(
            &client,
            &journal,
            Some(session),
            Start::Journal,
            Mode::Read,
            |value| {
                assert_eq!(journal.v2_feed_cursor(&scope).unwrap(), Some(3));
                if *value == answer {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(err, FeedError::Output(_)));
        drop(journal);
        let journal = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        let mut output = Vec::new();
        run(
            &client,
            &journal,
            Some(session),
            Start::Journal,
            Mode::Next,
            |value| {
                output.push(value.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output.iter().filter(|row| **row == chat).count(), 0);
        assert_eq!(output.iter().filter(|row| **row == answer).count(), 1);
        assert_eq!(
            output[0],
            json!({"kind":"baseline","reason":"cursor_expired","rev":40})
        );
        assert_eq!(output.last(), Some(&json!({"kind":"caught_up","rev":50})));
        assert_eq!(journal.v2_feed_cursor(&scope).unwrap(), Some(50));
        let requests = server.requests();
        assert!(requests[4].line.contains("&page=first "));
        assert!(requests[5].line.contains("&page=second "));
        assert!(requests[6].line.contains("after=40"));
        assert!(requests[7].line.contains("delivery-baseline"));
        assert!(requests[9].line.contains("after=50"));
    }

    fn rebuild(session: Uuid) -> Vec<FixtureResponse> {
        vec![
            expired(),
            baseline_unsupported(),
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
        assert!(requests[4].line.contains("before=entry-b"));
        assert!(requests[5].line.contains("page=later"));
        assert!(requests[6].line.contains("after=40"));
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
            baseline_unsupported(),
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
        assert!(server.requests()[2].line.contains("after=2"));
    }
    #[test]
    fn default_checkpoint_does_not_advance_when_output_is_interrupted() {
        let (_root, journal) = open();
        let baseline = SlowServer::sequence(vec![
            baseline_unsupported(),
            FixtureResponse::json(json!({"rev":40,"messages":[]})),
        ]);
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
