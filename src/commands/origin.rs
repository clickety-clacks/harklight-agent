use super::*;
use crate::api::OriginNotice;

pub fn cover(context: &CommandContext, session: Uuid) -> Result<(), CommandError> {
    let status = read_status(context, session)?;
    let support = &status["listening_support"];
    if support["route"] != "other"
        || !support["unsupported_reason"].is_null()
        || !support.is_object()
    {
        return unsupported("cover", "coverage requires a supported route-other handle");
    }
    let mut request = json!({"v":1,"type":"support","op_id":Uuid::new_v4(),
        "session_handle":session,"route":"other","cli_version":env!("CARGO_PKG_VERSION"),
        "unsupported_reason":null,"origin_listener":true});
    for key in ["ack2", "advisory_v1"] {
        if let Some(value) = support.get(key) {
            request[key] = value.clone();
        }
    }
    // This declaration takes no handle listener lease and changes no attachment.
    let mut store = context.open_store(session)?;
    let operation = prepare_or_resume_operation(&mut store, session, "support", request, false)?;
    let response = submit_operation(context, &mut store, operation)?;
    if response["listening_support"]["origin_listener"] != true {
        return unsupported("cover", "server did not confirm origin-listener coverage");
    }
    emit(
        &json!({"kind":"covered","session_handle":session,"listening_support":response["listening_support"]}),
    )
}

fn coverage(context: &CommandContext) -> Result<BTreeSet<Uuid>, CommandError> {
    let response = context
        .api
        .post(&json!({"v":1,"type":"coverage"}))
        .map_err(|error| api_error(error, EXIT_REFUSED))?;
    let handles = response["session_handles"]
        .as_array()
        .ok_or_else(|| CommandError::new("invalid coverage response", 1))?;
    handles
        .iter()
        .map(|handle| {
            handle
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| CommandError::new("invalid coverage handle", 1))
        })
        .collect()
}

fn emit_covered(
    store: &mut JournalStore,
    covered: &BTreeSet<Uuid>,
    offered: &mut BTreeSet<String>,
) -> Result<(), CommandError> {
    let pending = store
        .pending_for_coverage(covered)
        .map_err(|error| journal_error(error, 1))?;
    for (index, entry) in pending.iter().enumerate() {
        if !offered.contains(&entry.event_id) {
            emit(&event_line(entry, pending.len().saturating_sub(index + 1)))?;
            store
                .mark_emitted(&entry.event_id)
                .map_err(|error| journal_error(error, 1))?;
            offered.insert(entry.event_id.clone());
        }
    }
    Ok(())
}

pub fn listen_origin(context: &CommandContext) -> Result<(), CommandError> {
    let mut store = context.open_global_store()?;
    store
        .acquire_origin_listener()
        .map_err(|error| journal_error(error, EXIT_LISTENER_ACTIVE))?;
    let mut delay = Duration::from_millis(250);
    'connect: loop {
        let mut stream = match context.api.connect_origin_stream() {
            Ok(stream) => stream,
            Err(error)
                if error.refusal_reason().is_some_and(|r| {
                    matches!(
                        r.as_str(),
                        "not_found" | "invalid_field" | "not_supported" | "unsupported"
                    )
                }) || matches!(error, ApiError::Http { status: 404, .. }) =>
            {
                return unsupported("listen", "server does not support the origin stream");
            }
            Err(error) if stream_should_reconnect(&error) => {
                thread::sleep(delay);
                delay = delay.saturating_mul(2).min(Duration::from_secs(30));
                continue;
            }
            Err(error) => return Err(api_error(error, EXIT_REFUSED)),
        };
        let mut offered = BTreeSet::new();
        let mut covered = BTreeSet::new();
        let mut notice = None;
        loop {
            let refreshed = (|| {
                match notice {
                    None => {
                        covered = coverage(context)?;
                        fetch_feed(context, &mut store)?;
                    }
                    Some(OriginNotice::Coverage) => covered = coverage(context)?,
                    Some(OriginNotice::Change(cursor)) => {
                        if cursor > store.cursor().map_err(|error| journal_error(error, 1))? {
                            fetch_feed(context, &mut store)?;
                        }
                    }
                    Some(OriginNotice::Replaced) => {
                        return Err(CommandError::new(
                            "origin listener replaced",
                            EXIT_LISTENER_ACTIVE,
                        ));
                    }
                }
                // A shared reader can advance the cursor; emission remains independent of fetching.
                emit_covered(&mut store, &covered, &mut offered)
            })();
            match refreshed {
                Ok(()) => delay = Duration::from_millis(250),
                Err(error) if listener_post_failure_requires_reconnect(&error) => {
                    drop(stream);
                    thread::sleep(delay);
                    delay = delay.saturating_mul(2).min(Duration::from_secs(30));
                    continue 'connect;
                }
                Err(error) => return Err(error),
            }
            match stream.next_notice() {
                Ok(value) => notice = Some(value),
                Err(ApiError::StreamEnded | ApiError::StreamIo(_)) => break,
                Err(error) => return Err(api_error(error, 1)),
            }
        }
        drop(stream);
        thread::sleep(delay);
        delay = delay.saturating_mul(2).min(Duration::from_secs(30));
    }
}
