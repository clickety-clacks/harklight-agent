use super::*;

/// Only closed-vocabulary diagnostics cross the controller boundary. Error
/// Display implementations may contain URLs, paths or arbitrary response text.
#[derive(Debug, Clone)]
pub(super) struct SafeReason {
    code: &'static str,
    text: &'static str,
    source: &'static str,
}

impl SafeReason {
    fn local(code: &'static str, text: &'static str) -> Self {
        Self {
            code,
            text,
            source: "agent",
        }
    }

    fn unknown() -> Self {
        Self {
            code: "unknown",
            text: "The attempt failed without a recognized safe diagnostic.",
            source: "unknown",
        }
    }

    pub(super) fn server(reason: &str) -> Self {
        let (code, text) = match reason {
            "already_listening" => (
                "already_listening",
                "The origin handle already has a registered stream.",
            ),
            "not_found" => ("not_found", "The server did not find this origin handle."),
            "forbidden" => ("forbidden", "The server refused this origin's access."),
            "storage_unavailable" => (
                "storage_unavailable",
                "The server could not confirm durable storage.",
            ),
            "operation_conflict" => (
                "operation_conflict",
                "The server has different inputs for this operation key.",
            ),
            "invalid_field" => (
                "invalid_field",
                "The server rejected an attachment or support field.",
            ),
            "unsupported_version" => (
                "unsupported_version",
                "The server does not support this protocol version.",
            ),
            _ => return Self::unknown(),
        };
        Self {
            code,
            text,
            source: "server",
        }
    }

    pub(super) fn api(error: &ApiError) -> Self {
        match error {
            ApiError::Refused(reason) => Self::server(reason),
            ApiError::Http { body, .. } => serde_json::from_str::<Value>(body)
                .ok()
                .filter(|v| v["v"] == 1 && v["type"] == "error")
                .and_then(|v| v["reason"].as_str().map(Self::server))
                .unwrap_or_else(Self::unknown),
            ApiError::Transport(error) if error.is_timeout() => {
                Self::local("timeout", "The origin request timed out.")
            }
            ApiError::Transport(_) => {
                Self::local("transport_error", "The origin transport request failed.")
            }
            ApiError::StreamEnded => Self::local(
                "stream_ended",
                "The origin stream ended before the conversation became idle.",
            ),
            ApiError::StreamIo(_) => {
                Self::local("stream_io", "The origin stream could not be read.")
            }
            ApiError::InvalidNotice(_) => Self::local(
                "invalid_notice",
                "The origin stream supplied an invalid change notice.",
            ),
            ApiError::InvalidResponse(_) => Self::local(
                "invalid_response",
                "The origin response did not satisfy the protocol.",
            ),
            ApiError::Config(_) => Self::local(
                "configuration_error",
                "The enrolled origin configuration could not be loaded.",
            ),
            ApiError::Local(_) => {
                Self::local("local_io", "Local file access failed during the request.")
            }
        }
    }

    pub(super) fn journal(error: &JournalError) -> Self {
        match error {
            JournalError::ListenerActive(_) | JournalError::ListenerActiveUnknown => Self::local(
                "listener_active",
                "The existing local listener lock is held; no replacement was attempted.",
            ),
            JournalError::FeedGap { .. } => Self::local(
                "feed_gap",
                "The feed is not contiguous with the saved journal cursor.",
            ),
            JournalError::FeedConflict { .. } => Self::local(
                "feed_conflict",
                "A feed entry conflicts with the saved entry identity.",
            ),
            JournalError::OperationConflict(_) => Self::local(
                "operation_conflict",
                "The saved operation has different inputs.",
            ),
            _ => Self::local(
                "journal_error",
                "The existing origin journal could not complete the requested operation.",
            ),
        }
    }
}

#[derive(Debug)]
struct Failure {
    stage: &'static str,
    operation: String,
    op_id: Option<Uuid>,
    exit_code: i32,
    reason: SafeReason,
}

impl Failure {
    fn command(stage: &'static str, error: CommandError) -> Self {
        Self {
            stage,
            operation: "reconnect".into(),
            op_id: None,
            exit_code: error.exit_code,
            reason: error.safe_reason.unwrap_or_else(SafeReason::unknown),
        }
    }

    fn local(stage: &'static str, code: &'static str, text: &'static str, exit_code: i32) -> Self {
        Self {
            stage,
            operation: "reconnect".into(),
            op_id: None,
            exit_code,
            reason: SafeReason::local(code, text),
        }
    }

    fn line(&self, session: Uuid) -> Value {
        let kind = match self.exit_code {
            EXIT_OUTCOME_UNKNOWN => "outcome_unknown",
            EXIT_REFUSED | EXIT_UNSUPPORTED | EXIT_LISTENER_ACTIVE => "refused",
            _ => "connection_error",
        };
        let mut line = json!({"kind":kind,"operation":self.operation,"session_handle":session,
            "stage":self.stage,"reason_code":self.reason.code,"reason":self.reason.text,
            "reason_source":self.reason.source});
        if let Some(op_id) = self.op_id {
            line["op_id"] = json!(op_id);
        }
        line
    }

    fn report(self, session: Uuid) -> CommandError {
        // A broken output pipe must not fall back to printing raw diagnostics.
        let _ = emit(&self.line(session));
        CommandError::new(self.reason.text, self.exit_code)
    }
}

pub fn reconnect_configuration_error(session: Uuid, operation: &str) -> CommandError {
    let mut failure = Failure::local(
        "journal",
        "configuration_error",
        "The enrolled origin configuration could not be loaded.",
        1,
    );
    failure.operation = operation.to_owned();
    failure.report(session)
}

fn stage_for(operation: &OperationRecord) -> &'static str {
    if operation.kind == "attachment_update" {
        "attachment"
    } else {
        "support"
    }
}

fn submit(
    transport: &impl OperationTransport,
    store: &mut JournalStore,
    operation: OperationRecord,
) -> Result<Value, Failure> {
    let stage = stage_for(&operation);
    let op_id = operation.op_id;
    let kind = operation.kind.clone();
    submit_operation_inner(transport, store, operation, false).map_err(|error| {
        let mut failure = Failure::command(stage, error);
        failure.op_id = Some(op_id);
        failure.operation = kind;
        failure
    })
}

fn recover_attachment(
    transport: &impl OperationTransport,
    store: &mut JournalStore,
    session: Uuid,
) -> Result<(), Failure> {
    let operations = store
        .operations_for_session(session)
        .map_err(|e| Failure::command("journal", journal_error(e, 1)))?;
    // Preserve receipt ordering and exact payloads, including support prepared
    // by the standalone listener. Never replay a send, reply or result here.
    for operation in operations.into_iter().filter(|op| {
        matches!(
            op.kind.as_str(),
            "attachment_update" | "attachment_support" | "support"
        ) && matches!(op.state, OperationState::Prepared | OperationState::Unknown)
    }) {
        submit(transport, store, operation)?;
    }
    Ok(())
}

fn saved_attachment(
    store: &JournalStore,
    session: Uuid,
) -> Result<(String, OriginHarness, bool), Failure> {
    let route = attachment_route_for_listen(store, session)
        .map_err(|e| Failure::command("attachment", e))?
        .ok_or_else(|| Failure::local("attachment", "missing_attachment",
            "No confirmed saved attachment is available; provide the existing vendor ID and route.", EXIT_REFUSED))?;
    let operations = store
        .operations_for_session(session)
        .map_err(|e| Failure::command("journal", journal_error(e, 1)))?;
    let attachment = operations
        .iter()
        .rev()
        .find(|op| op.kind == "attachment_update")
        .ok_or_else(|| {
            Failure::local(
                "attachment",
                "missing_attachment",
                "No confirmed attachment is available.",
                EXIT_REFUSED,
            )
        })?;
    let vendor = match &attachment.request["vendor_session_id"] {
        Value::Null => "none".to_owned(),
        Value::String(value) if !value.is_empty() => value.clone(),
        _ => {
            return Err(Failure::local(
                "attachment",
                "invalid_attachment",
                "The saved vendor attachment is invalid.",
                EXIT_REFUSED,
            ));
        }
    };
    let route = match route.as_str() {
        "other" => OriginHarness::Other,
        "claude-code" => OriginHarness::ClaudeCode,
        "codex" => OriginHarness::Codex,
        _ => {
            return Err(Failure::local(
                "support",
                "unsupported_route",
                "The saved attachment route is unsupported.",
                EXIT_UNSUPPORTED,
            ));
        }
    };
    let support = operations
        .iter()
        .rev()
        .find(|op| op.kind == "attachment_support")
        .ok_or_else(|| {
            Failure::local(
                "support",
                "missing_support",
                "No saved attachment support is available.",
                EXIT_UNSUPPORTED,
            )
        })?;
    Ok((
        vendor,
        route,
        route == OriginHarness::Other && support.request["unsupported_reason"].is_null(),
    ))
}

fn attach_locked(
    context: &CommandContext,
    store: &mut JournalStore,
    session: Uuid,
    vendor: &str,
    route: OriginHarness,
    controller: bool,
) -> Result<Value, Failure> {
    if vendor.is_empty() || (controller && route != OriginHarness::Other) {
        return Err(Failure::local(
            "attachment",
            "invalid_attachment",
            "Use a non-empty vendor ID or none; controller requires route other.",
            EXIT_REFUSED,
        ));
    }
    let vendor = (vendor != "none").then_some(vendor);
    let support = attachment_support_request(Uuid::new_v4(), session, route, controller);
    ensure_attach_recovery_compatible(store, session, vendor, &support)
        .map_err(|e| Failure::command("attachment", e))?;
    let operation = prepare_or_resume_operation(
        store,
        session,
        "attachment_update",
        attachment_update_request(Uuid::new_v4(), session, vendor),
        true,
    )
    .map_err(|e| Failure::command("attachment", e))?;
    let attachment_op_id = operation.op_id;
    let request = operation.request.clone();
    let response = submit(&context.api, store, operation)?;
    if !attachment_update_response_matches(&request, &response) {
        return Err(Failure::local(
            "attachment",
            "invalid_attachment_receipt",
            "The saved receipt does not confirm this handle and vendor ID.",
            EXIT_REFUSED,
        ));
    }
    let operation =
        prepare_or_resume_operation(store, session, "attachment_support", support.clone(), false)
            .map_err(|e| Failure::command("support", e))?;
    let support_op_id = operation.op_id;
    submit(&context.api, store, operation)?;
    let status = read_status(context, session).map_err(|e| Failure::command("support", e))?;
    if !support_record_matches(&status, &support) {
        return Err(Failure::local(
            "support",
            "support_mismatch",
            "The origin did not confirm the requested route support.",
            EXIT_UNSUPPORTED,
        ));
    }
    Ok(
        json!({"kind":"attached","session_handle":session,"vendor_session_id":vendor,
        "attachment_op_id":attachment_op_id,"support_op_id":support_op_id,
        "ready_for_listen":support["unsupported_reason"].is_null(),
        "listening_support":status["listening_support"]}),
    )
}

fn open_locked(context: &CommandContext, session: Uuid) -> Result<JournalStore, Failure> {
    if session.get_version() != Some(Version::Random) {
        return Err(Failure::local(
            "journal",
            "invalid_handle",
            "An existing UUIDv4 handle is required.",
            EXIT_REFUSED,
        ));
    }
    let mut store = context
        .open_store(session)
        .map_err(|e| Failure::command("journal", e))?;
    store
        .acquire_listener()
        .map_err(|e| Failure::command("journal", journal_error(e, 1)))?;
    if !local_journal_proves_existing_session(&store, session)
        .map_err(|e| Failure::command("journal", e))?
    {
        return Err(Failure::local(
            "journal",
            "unproven_handle",
            "The same-origin local journal does not prove this existing handle.",
            EXIT_REFUSED,
        ));
    }
    Ok(store)
}

pub(super) fn controller_attach(
    context: &CommandContext,
    session: Uuid,
    vendor: &str,
    route: OriginHarness,
) -> Result<(), CommandError> {
    let result = (|| {
        let mut store = open_locked(context, session)?;
        recover_attachment(&context.api, &mut store, session)?;
        let attached = attach_locked(context, &mut store, session, vendor, route, true)?;
        emit(&attached).map_err(|e| Failure::command("attachment", e))
    })();
    result.map_err(|e: Failure| e.report(session))
}

pub fn reconnect(
    context: &CommandContext,
    session: Uuid,
    vendor: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
) -> Result<(), CommandError> {
    reconnect_inner(context, session, vendor, route, controller).map_err(|e| e.report(session))
}

fn reconnect_inner(
    context: &CommandContext,
    session: Uuid,
    vendor: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
) -> Result<(), Failure> {
    let mut store = open_locked(context, session)?;
    recover_attachment(&context.api, &mut store, session)?;
    let (vendor, route, controller) = match (vendor, route) {
        (Some(vendor), Some(route)) => (vendor.to_owned(), route, controller),
        (None, None) if !controller => saved_attachment(&store, session)?,
        _ => {
            return Err(Failure::local(
                "attachment",
                "missing_attachment",
                "Provide both vendor ID and route, or use the confirmed saved attachment.",
                EXIT_REFUSED,
            ));
        }
    };
    let attached = attach_locked(context, &mut store, session, &vendor, route, controller)?;
    if attached["ready_for_listen"] != true {
        return Err(Failure::local(
            "support",
            "unsupported_route",
            "The requested attachment route is not supported for listening.",
            EXIT_UNSUPPORTED,
        ));
    }
    // The local listener lock remains held throughout the real SSE connection.
    // There is no private retry budget in this worker.
    let mut stream = context.api.reconnect_stream(session).map_err(|e| {
        let exit = if matches!(&e, ApiError::Refused(reason) if reason == "already_listening") {
            EXIT_LISTENER_ACTIVE
        } else if matches!(&e, ApiError::Refused(_)) {
            EXIT_REFUSED
        } else {
            1
        };
        Failure::command("stream", api_error(e, exit))
    })?;
    // Consume the initial change notice before declaring readiness. Its cursor
    // is only a trigger; the durable journal cursor comes from the feed.
    stream
        .next_change()
        .map_err(|e| Failure::command("stream", api_error(e, 1)))?;
    fetch_feed(context, &mut store).map_err(|e| Failure::command("feed", e))?;
    let status = read_status(context, session).map_err(|e| Failure::command("feed", e))?;
    let mut offered = BTreeSet::new();
    if session_idle(&status).map_err(|e| Failure::command("feed", e))? {
        emit_pending(&mut store, session, true, &mut offered)
            .map_err(|e| Failure::command("feed", e))?;
        return emit(&json!({"kind":"idle","open_requests":0}))
            .map_err(|e| Failure::command("feed", e));
    }
    let cursor = store
        .cursor()
        .map_err(|e| Failure::command("journal", journal_error(e, 1)))?;
    emit(&json!({"kind":"reconnected","session_handle":session,"feed_cursor":cursor}))
        .map_err(|e| Failure::command("feed", e))?;
    emit_pending(&mut store, session, true, &mut offered)
        .map_err(|e| Failure::command("feed", e))?;
    loop {
        let notice = stream
            .next_change()
            .map_err(|e| Failure::command("stream", api_error(e, 1)))?;
        let cursor = store
            .cursor()
            .map_err(|e| Failure::command("journal", journal_error(e, 1)))?;
        if notice <= cursor {
            continue;
        }
        fetch_feed(context, &mut store).map_err(|e| Failure::command("feed", e))?;
        let status = read_status(context, session).map_err(|e| Failure::command("feed", e))?;
        emit_pending(&mut store, session, true, &mut offered)
            .map_err(|e| Failure::command("feed", e))?;
        if session_idle(&status).map_err(|e| Failure::command("feed", e))? {
            return emit(&json!({"kind":"idle","open_requests":0}))
                .map_err(|e| Failure::command("feed", e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Transport {
        requests: RefCell<Vec<Value>>,
        refusal: Option<&'static str>,
    }

    impl OperationTransport for Transport {
        fn post(&self, request: &Value) -> Result<Value, ApiError> {
            self.requests.borrow_mut().push(request.clone());
            if let Some(reason) = self.refusal {
                return Ok(
                    json!({"v":1,"type":"error","reason":reason,"private":"not for output"}),
                );
            }
            Ok(
                json!({"v":1,"type":"ok","session_handle":request["session_handle"],
                "vendor_session_id":request["vendor_session_id"]}),
            )
        }
        fn upload_file(
            &self,
            _: Uuid,
            _: &str,
            _: &str,
            _: u64,
            _: &str,
            _: &Path,
            _: u64,
        ) -> Result<Value, ApiError> {
            panic!("attachment recovery must not upload human action files")
        }
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            Self(env::temp_dir().join(format!("reconnect-test-{}", Uuid::new_v4())))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exact_attachment_recovery_does_not_replay_human_actions() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "fixture-device", session).unwrap();
        let attachment =
            attachment_update_request(Uuid::new_v4(), session, Some("original-vendor"));
        let op_id = parse_uuid_field(&attachment, "op_id").unwrap();
        prepare(
            &mut store,
            session,
            op_id,
            "attachment_update",
            attachment.clone(),
        )
        .unwrap();
        store.mark_operation_unknown(op_id).unwrap();
        let action_id = Uuid::new_v4();
        prepare(
            &mut store,
            session,
            action_id,
            "reply",
            json!({"op_id":action_id,"text":"original action"}),
        )
        .unwrap();
        store.mark_operation_unknown(action_id).unwrap();
        let transport = Transport {
            requests: RefCell::new(vec![]),
            refusal: None,
        };
        recover_attachment(&transport, &mut store, session).unwrap();
        assert_eq!(*transport.requests.borrow(), vec![attachment]);
        assert_eq!(
            store.operation(op_id).unwrap().unwrap().state,
            OperationState::Committed
        );
        assert_eq!(
            store.operation(action_id).unwrap().unwrap().state,
            OperationState::Unknown
        );
        recover_attachment(&transport, &mut store, session).unwrap();
        assert_eq!(transport.requests.borrow().len(), 1);
    }

    #[test]
    fn storage_refusal_preserves_exact_unknown_operation_and_safe_reason() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "fixture-device", session).unwrap();
        let request = attachment_update_request(Uuid::new_v4(), session, None);
        let op_id = parse_uuid_field(&request, "op_id").unwrap();
        prepare(
            &mut store,
            session,
            op_id,
            "attachment_update",
            request.clone(),
        )
        .unwrap();
        let transport = Transport {
            requests: RefCell::new(vec![]),
            refusal: Some("storage_unavailable"),
        };
        let failure = recover_attachment(&transport, &mut store, session).unwrap_err();
        let line = failure.line(session);
        assert_eq!(failure.exit_code, EXIT_OUTCOME_UNKNOWN);
        assert_eq!(line["kind"], "outcome_unknown");
        assert_eq!(line["op_id"], op_id.to_string());
        assert_eq!(line["reason_code"], "storage_unavailable");
        assert_eq!(line["reason_source"], "server");
        assert!(!line.to_string().contains("private"));
        let saved = store.operation(op_id).unwrap().unwrap();
        assert_eq!(saved.state, OperationState::Unknown);
        assert_eq!(saved.request, request);
    }

    #[test]
    fn arbitrary_response_details_never_become_controller_diagnostics() {
        let session = Uuid::new_v4();
        for error in [
            ApiError::Refused("secret /private/path".into()),
            ApiError::Http {
                status: 500,
                body: "secret /private/path".into(),
            },
            ApiError::InvalidResponse("secret /private/path".into()),
        ] {
            let line = Failure::command("stream", api_error(error, 1)).line(session);
            assert!(!line.to_string().contains("secret"));
            assert!(!line.to_string().contains("/private/path"));
            assert!(line.get("op_id").is_none());
        }
        let line = Failure::command(
            "stream",
            api_error(ApiError::Refused("unrecognized".into()), EXIT_REFUSED),
        )
        .line(session);
        assert_eq!(line["reason_code"], "unknown");
        assert_eq!(line["reason_source"], "unknown");
    }
}
