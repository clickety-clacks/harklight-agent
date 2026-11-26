use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::journal::{DeliveryAttempt, DeliveryPages, JournalStore};
use crate::v2::{AgentClient, V2Error};

use super::{Change, FeedError, emit_change, emit_value};

pub(super) struct DeliveredBaseline {
    pub rev: u64,
    pub ack2: bool,
    pub changes: Vec<Change>,
}

/// A supported delivery baseline emits pending turns in their original order.
/// The original token remains the recoverable start of the attempt.
pub(super) fn delivery_baseline(
    client: &AgentClient,
    journal: &JournalStore,
    session: Option<Uuid>,
    key: &str,
    reason: &str,
    emitted_turns: &mut BTreeMap<String, Change>,
    output: &mut impl FnMut(&Value) -> io::Result<()>,
) -> Result<Option<DeliveredBaseline>, FeedError> {
    let attempt = match journal.delivery_attempt(key)? {
        Some(attempt) if !attempt.renew => attempt,
        previous => {
            let continued = previous.is_some();
            let ack2 = previous
                .as_ref()
                .map_or(client.ack2, |attempt| attempt.ack2);
            let mut prior = Vec::new();
            if let Some(previous) = previous {
                prior = previous.prior;
                prior.push(DeliveryPages {
                    rev: previous.rev,
                    first_page: previous.first_page,
                    ack2: previous.ack2,
                });
            }
            let start = match client.delivery_page(session, None, ack2) {
                Ok(start) => start,
                Err(FeedError::Api(V2Error::Http {
                    status: 404,
                    token: Some(token),
                    ..
                })) if !continued && token == "not_found" => return Ok(None),
                Err(error) => return Err(error),
            };
            let attempt = DeliveryAttempt {
                rev: start["rev"].as_u64().ok_or(FeedError::InvalidSnapshot)?,
                first_page: next_delivery_page(&start)?
                    .ok_or(FeedError::InvalidSnapshot)?
                    .to_owned(),
                ack2,
                expired: continued || reason == "cursor_expired",
                renew: false,
                prior,
            };
            journal.save_delivery_attempt(key, &attempt)?;
            attempt
        }
    };
    let reason = if attempt.expired {
        "cursor_expired"
    } else {
        "first_contact"
    };
    let rev = attempt.rev;
    emit_value(output, json!({"kind":"baseline","rev":rev,"reason":reason}))?;
    // Follow retains the emitted turns if fetching the rest of this
    // baseline or its first catch-up batch fails and must be retried.
    let current = DeliveryPages {
        rev,
        first_page: attempt.first_page.clone(),
        ack2: attempt.ack2,
    };
    for pages in attempt.prior.iter().chain(std::iter::once(&current)) {
        let mut page = pages.first_page.clone();
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(page.clone()) {
                return Err(FeedError::InvalidSnapshot);
            }
            let response = client.delivery_page(session, Some(&page), pages.ack2)?;
            if response["rev"] != pages.rev {
                return Err(FeedError::InvalidSnapshot);
            }
            let changes: Vec<Change> = serde_json::from_value(response["changes"].clone())
                .map_err(|_| FeedError::InvalidSnapshot)?;
            if changes
                .iter()
                .any(|change| !valid_pending_change(change, session))
            {
                return Err(FeedError::InvalidSnapshot);
            }
            let next = next_delivery_page(&response)?;
            for change in &changes {
                let id = change.resource_key()?;
                let previous = emitted_turns.get(&id);
                if previous.is_some_and(|prior| {
                    prior.rev > change.rev || same_baseline_content(prior, change)
                }) {
                    continue;
                }
                if previous.is_some() || settled_turn(change) {
                    emit_value(
                        output,
                        json!({
                            "kind":change.kind,"rev":change.rev,"data":change.data,"pending":false,
                        }),
                    )?;
                } else {
                    emit_change(output, change)?;
                }
                emitted_turns.insert(id, change.clone());
            }
            match next {
                Some(next) => page = next.to_owned(),
                None => break,
            }
        }
    }
    let changes: Vec<_> = emitted_turns.values().cloned().collect();
    emit_value(output, json!({"kind":"baseline_complete","rev":rev}))?;
    Ok(Some(DeliveredBaseline {
        rev,
        ack2: attempt.ack2,
        changes,
    }))
}

pub(super) fn next_delivery_page(response: &Value) -> Result<Option<&str>, FeedError> {
    match &response["next_page"] {
        Value::Null => Ok(None),
        Value::String(token) if !token.is_empty() => Ok(Some(token)),
        _ => Err(FeedError::InvalidSnapshot),
    }
}

pub(super) fn valid_pending_change(change: &Change, session: Option<Uuid>) -> bool {
    if change.resource_key().is_err()
        || session.is_some_and(|session| {
            (change.kind == "message" || change.data.get("session").is_some())
                && change.data["session"] != session.to_string()
        })
    {
        return false;
    }
    match change.kind.as_str() {
        "entry" => {
            change.data["author"] == "person"
                && change.data["kind"] == "reply"
                && change.data["item_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
        }
        "message" => {
            change.data.get("shared").is_none() && change.data["terminal"]["state"] == "answered"
        }
        "answer" => {
            change.data["terminal"]["state"] == "answered"
                && change.data["terminal"]["answer_id"] == change.data["answer_id"]
                && change.data["item_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
        }
        _ => false,
    }
}

pub(super) fn settled_turn(change: &Change) -> bool {
    let delivery = if matches!(change.kind.as_str(), "message" | "answer") {
        &change.data["terminal"]["delivery"]
    } else {
        &change.data["delivery"]
    };
    delivery["acted"] == true || !delivery["outcome"].is_null()
}

fn same_baseline_content(prior: &Change, current: &Change) -> bool {
    let without_ack1 = |change: &Change| {
        let mut data = change.data.clone();
        data.as_object_mut()
            .expect("validated feed data")
            .remove("rev");
        let turn = if change.kind == "message" {
            data.get_mut("terminal").and_then(Value::as_object_mut)
        } else {
            data.as_object_mut()
        };
        if let Some(turn) = turn
            && let Some(delivery) = turn.get_mut("delivery").and_then(Value::as_object_mut)
        {
            delivery.remove("delivered");
            if delivery.is_empty() {
                turn.remove("delivery");
            }
        }
        data
    };
    // A later representation can add ACK1 and a resource revision. Any
    // other projected change still needs to reach the consumer.
    without_ack1(prior) == without_ack1(current)
}

pub(super) fn baseline_replay(
    journal: &JournalStore,
    key: &str,
    change: &Change,
) -> Result<bool, FeedError> {
    if !valid_pending_change(change, None) {
        return Ok(false);
    }
    Ok(journal
        .baseline_change(key, change)?
        .is_some_and(|prior| prior.rev >= change.rev || same_baseline_content(&prior, change)))
}
