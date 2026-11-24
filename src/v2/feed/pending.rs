use std::{collections::BTreeSet, io};

use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use super::{
    AgentClient, Change, FeedError, V2Error,
    delivery::{next_delivery_page, settled_turn, valid_pending_change},
    emit_change, emit_value,
};

#[derive(Debug, Error)]
pub enum PendingError {
    #[error("pending baseline is unsupported")]
    Unsupported,
    #[error("pending baseline token rejected")]
    TokenInvalid {
        reason: String,
        field: Option<String>,
        request_id: Option<String>,
    },
    #[error(transparent)]
    Feed(#[from] FeedError),
}

impl PendingError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Unsupported => 5,
            Self::TokenInvalid { .. } => 2,
            Self::Feed(error) => error.exit_code(),
        }
    }

    pub fn machine_result(&self) -> Option<Value> {
        match self {
            Self::Unsupported => Some(json!({"kind":"unsupported","operation":"pending_baseline"})),
            Self::TokenInvalid {
                reason,
                field,
                request_id,
            } => Some(json!({
                "kind":"token_invalid", "reason":reason, "field":field, "request_id":request_id,
            })),
            Self::Feed(_) => None,
        }
    }
}

pub fn run(
    client: &AgentClient,
    session: Uuid,
    token: Option<&str>,
    mut output: impl FnMut(&Value) -> io::Result<()>,
) -> Result<(), PendingError> {
    let mut response = page(client, session, token)?;
    let rev = response["rev"].as_u64().ok_or(FeedError::InvalidSnapshot)?;
    let Some(token) = token else {
        let token = next_delivery_page(&response)?.ok_or(FeedError::InvalidSnapshot)?;
        return Ok(emit_value(
            &mut output,
            json!({
                "kind":"baseline_prepared", "session":session, "rev":rev, "token":token,
            }),
        )?);
    };

    let mut visited = BTreeSet::from([token.to_owned()]);
    let mut opened = false;
    loop {
        if response["rev"] != rev {
            return Err(FeedError::InvalidSnapshot.into());
        }
        let changes: Vec<Change> = serde_json::from_value(response["changes"].take())
            .map_err(|_| FeedError::InvalidSnapshot)?;
        if changes
            .iter()
            .any(|change| !valid_pending_change(change, Some(session)))
        {
            return Err(FeedError::InvalidSnapshot.into());
        }
        let next = next_delivery_page(&response)?;
        if next.is_some_and(|next| !visited.insert(next.to_owned())) {
            return Err(FeedError::InvalidSnapshot.into());
        }
        if !opened {
            emit_value(
                &mut output,
                json!({"kind":"baseline","rev":rev,"reason":"first_contact"}),
            )?;
            opened = true;
        }
        for change in &changes {
            if settled_turn(change) {
                emit_value(
                    &mut output,
                    json!({
                        "kind":change.kind,"rev":change.rev,"data":change.data,"pending":false,
                    }),
                )?;
            } else {
                emit_change(&mut output, change)?;
            }
        }
        let Some(next) = next else { break };
        response = page(client, session, Some(next))?;
    }
    Ok(emit_value(
        &mut output,
        json!({"kind":"baseline_complete","rev":rev}),
    )?)
}

fn page(client: &AgentClient, session: Uuid, token: Option<&str>) -> Result<Value, PendingError> {
    match client.delivery_page(Some(session), token, client.ack2) {
        Err(FeedError::Api(V2Error::Http {
            status,
            token: Some(reason),
            ..
        })) if (status == 404 && reason == "not_found")
            || (status == 400 && reason == "unsupported") =>
        {
            Err(PendingError::Unsupported)
        }
        Err(FeedError::Api(V2Error::Http {
            status: 400,
            token: Some(reason),
            field,
            request_id,
            ..
        })) if token.is_some() && reason == "invalid_field" && field.as_deref() == Some("page") => {
            Err(PendingError::TokenInvalid {
                reason,
                field,
                request_id,
            })
        }
        result => Ok(result?),
    }
}
