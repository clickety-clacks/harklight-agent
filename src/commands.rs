use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    path::Path,
};
use thiserror::Error;

use crate::journal::now_ms;

pub const EXIT_REFUSED: i32 = 2;
const ANSWER_UI_SCHEMA: &str = "attention-client.actions/1";
mod progress;
pub use progress::ProgressReport;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct CommandError {
    pub message: String,
    pub exit_code: i32,
}

impl CommandError {
    fn new(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFieldOptions {
    pub message_type: Option<String>,
    pub message_kind: Option<String>,
    pub reply: bool,
    pub importance: Option<String>,
    pub by_when: Option<String>,
    pub stake: Option<String>,
    pub thread: Option<String>,
    pub display_name: Option<String>,
    pub default_choice: Option<String>,
    pub default_at: Option<String>,
    pub choice_styles: Vec<String>,
    pub clear_default: bool,
    /// An exact sender-provided UI/1 document accepted only by send.
    pub answer_ui: Option<Value>,
    /// Makes a send a Progress FYI carrying this first report.
    pub progress: Option<ProgressReport>,
}

const SENDER_ANSWER_UI_SCHEMA: &str = "attention-client.ui/1";

#[derive(Debug)]
struct UniqueJsonMembers;

impl<'de> Deserialize<'de> for UniqueJsonMembers {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UniqueJsonMembersVisitor;

        impl<'de> Visitor<'de> for UniqueJsonMembersVisitor {
            type Value = UniqueJsonMembers;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value with no repeated object member names")
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_i128<E>(self, _value: i128) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_u128<E>(self, _value: u128) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJsonMembers)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while sequence.next_element::<UniqueJsonMembers>()?.is_some() {}
                Ok(UniqueJsonMembers)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut names = HashSet::new();
                while let Some(name) = map.next_key::<String>()? {
                    if !names.insert(name.clone()) {
                        return Err(de::Error::custom(format!("repeated member name {name:?}")));
                    }
                    map.next_value::<UniqueJsonMembers>()?;
                }
                Ok(UniqueJsonMembers)
            }
        }

        deserializer.deserialize_any(UniqueJsonMembersVisitor)
    }
}

pub(crate) fn read_answer_ui_file(path: &Path) -> Result<Value, CommandError> {
    let source = std::fs::read_to_string(path).map_err(|error| {
        CommandError::new(
            format!(
                "--answer-ui file {} could not be read: {error}; fix the path, or drop --answer-ui and use --choice/--reply",
                path.display()
            ),
            EXIT_REFUSED,
        )
    })?;

    let mut deserializer = serde_json::Deserializer::from_str(&source);
    UniqueJsonMembers::deserialize(&mut deserializer).map_err(|error| {
        CommandError::new(
            format!(
                "--answer-ui file {} is not valid JSON with unique member names: {error}; fix the JSON, or drop --answer-ui and use --choice/--reply",
                path.display()
            ),
            EXIT_REFUSED,
        )
    })?;
    deserializer.end().map_err(|error| {
        CommandError::new(
            format!(
                "--answer-ui file {} is not valid JSON: {error}; fix the JSON, or drop --answer-ui and use --choice/--reply",
                path.display()
            ),
            EXIT_REFUSED,
        )
    })?;

    let value: Value = serde_json::from_str(&source).map_err(|error| {
        CommandError::new(
            format!(
                "--answer-ui file {} is not valid JSON: {error}; fix the JSON, or drop --answer-ui and use --choice/--reply",
                path.display()
            ),
            EXIT_REFUSED,
        )
    })?;
    if !value.is_object() || value["schema"] != SENDER_ANSWER_UI_SCHEMA {
        return Err(CommandError::new(
            format!(
                "--answer-ui file {} must be a JSON object with schema \"{SENDER_ANSWER_UI_SCHEMA}\"; fix the file, or drop --answer-ui and use --choice/--reply",
                path.display()
            ),
            EXIT_REFUSED,
        ));
    }
    Ok(value)
}

#[derive(Debug, Clone)]
struct ChoiceTarget {
    id: String,
    value: Value,
}

#[derive(Debug, Default)]
struct NormalizedMessageFields {
    members: Map<String, Value>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

fn send_choice_targets(choices: &[String]) -> BTreeMap<String, ChoiceTarget> {
    choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            (
                choice.clone(),
                ChoiceTarget {
                    id: format!("choice-{}", index + 1),
                    value: Value::String(choice.clone()),
                },
            )
        })
        .collect()
}

fn answer_ui_choice_targets(answer_ui: &Value) -> BTreeMap<String, ChoiceTarget> {
    let mut targets = BTreeMap::new();
    if let Some(actions) = answer_ui["actions"].as_array() {
        for action in actions {
            if action["kind"] == "value" {
                if let (Some(id), Some(label), Some(value)) = (
                    action["id"].as_str(),
                    action["label"].as_str(),
                    action.get("value"),
                ) {
                    targets
                        .entry(label.to_owned())
                        .or_insert_with(|| ChoiceTarget {
                            id: id.to_owned(),
                            value: value.clone(),
                        });
                }
            }
        }
    }
    targets
}

fn normalize_message_fields(
    options: &MessageFieldOptions,
    choices: &BTreeMap<String, ChoiceTarget>,
    include_thread: bool,
) -> NormalizedMessageFields {
    let mut normalized = NormalizedMessageFields::default();

    if let Some(importance) = options.importance.as_deref() {
        if matches!(importance, "low" | "normal" | "high" | "now") {
            normalized
                .members
                .insert("urgency".into(), Value::String(importance.to_owned()));
        } else {
            normalized.errors.push(format!(
                "--importance must be one of low, normal, high, now; got {importance:?}"
            ));
        }
    }

    match (&options.default_choice, &options.default_at) {
        (None, None) => {}
        (Some(_), None) => normalized
            .warnings
            .push("--default needs --default-at; leaving the default out".into()),
        (None, Some(_)) => normalized
            .warnings
            .push("--default-at needs --default; leaving the default out".into()),
        (Some(choice), Some(at)) => {
            let target = choices.get(choice);
            let timestamp = parse_rfc3339_ms(at);

            if options.clear_default {
                normalized.warnings.push(
                    "--default conflicts with --clear-default; leaving the default out".into(),
                );
            } else {
                if target.is_none() {
                    normalized.warnings.push(format!(
                        "--default choice {choice:?} does not name a choice; leaving the default out"
                    ));
                }
                if timestamp.is_none() {
                    normalized.warnings.push(format!(
                        "--default-at value {at:?} is not a valid RFC 3339 time; leaving the default out"
                    ));
                }
                if let (Some(target), Some(timestamp)) = (target, timestamp) {
                    normalized.members.insert(
                        "default".into(),
                        json!({"value":target.value,"at":timestamp}),
                    );
                }
            }
        }
    }

    if let Some(by_when) = options.by_when.as_deref() {
        match parse_rfc3339_ms(by_when) {
            Some(timestamp) => {
                normalized
                    .members
                    .insert("by_when".into(), Value::from(timestamp));
            }
            None => normalized.errors.push(format!(
                "--by-when must be a valid RFC 3339 time; got {by_when:?} (for example, 2026-11-01T17:00:00Z)"
            )),
        }
    }

    if include_thread {
        if let Some(thread) = options.thread.as_deref() {
            if !thread.is_empty() {
                normalized
                    .members
                    .insert("thread".into(), Value::String(thread.to_owned()));
            } else {
                normalized
                    .warnings
                    .push("--thread is empty; leaving thread out".into());
            }
        }
    }

    if !options.choice_styles.is_empty() {
        let mut styles = Map::new();
        for style_option in &options.choice_styles {
            let Some((choice_text, style)) = style_option.rsplit_once('=') else {
                normalized.warnings.push(format!(
                    "--choice-style value {style_option:?} must end with =STYLE; leaving this style out"
                ));
                continue;
            };
            if !matches!(style, "primary" | "secondary" | "destructive") {
                normalized.warnings.push(format!(
                    "--choice-style value {style_option:?} has an unsupported style; leaving this style out"
                ));
                continue;
            }
            let Some(target) = choices.get(choice_text) else {
                normalized.warnings.push(format!(
                    "--choice-style choice {choice_text:?} does not name a choice; leaving this style out"
                ));
                continue;
            };
            styles.insert(target.id.clone(), Value::String(style.to_owned()));
        }
        if !styles.is_empty() {
            normalized
                .members
                .insert("choice_styles".into(), Value::Object(styles));
        }
    }

    if let Some(stake) = options.stake.as_deref() {
        if !stake.is_empty() {
            normalized
                .members
                .insert("stake".into(), Value::String(stake.to_owned()));
        } else {
            normalized
                .warnings
                .push("--stake is empty; leaving stake out".into());
        }
    }

    normalized
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageType {
    Fyi,
    Request,
}

impl MessageType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fyi => "fyi",
            Self::Request => "request",
        }
    }
}

fn parse_message_type(value: &str) -> Result<MessageType, CommandError> {
    match value {
        "fyi" => Ok(MessageType::Fyi),
        "request" => Ok(MessageType::Request),
        _ => Err(CommandError::new(
            format!("--type must be fyi or request; got {value:?}"),
            EXIT_REFUSED,
        )),
    }
}

fn send_message_type(fields: &MessageFieldOptions) -> Result<MessageType, CommandError> {
    match fields.message_type.as_deref() {
        Some(message_type) => parse_message_type(message_type),
        None => Err(CommandError::new(
            "send requires --type fyi or --type request",
            EXIT_REFUSED,
        )),
    }
}

fn revise_message_type(
    current: &Value,
    fields: &MessageFieldOptions,
) -> Result<Option<MessageType>, CommandError> {
    if let Some(message_type) = fields.message_type.as_deref() {
        return parse_message_type(message_type).map(Some);
    }
    match current.get("message_type") {
        None => Ok(None),
        Some(Value::String(message_type)) => parse_message_type(message_type).map(Some),
        Some(_) => Err(CommandError::new(
            "stored message_type is invalid; revise with --type fyi or --type request",
            EXIT_REFUSED,
        )),
    }
}

fn validate_message_kind(fields: &MessageFieldOptions) -> Result<(), CommandError> {
    if let Some(kind) = fields.message_kind.as_deref()
        && !matches!(kind, "blocker" | "success")
    {
        return Err(CommandError::new(
            format!("--kind must be blocker or success, or left out; got {kind:?}"),
            EXIT_REFUSED,
        ));
    }
    Ok(())
}

fn validate_send_controls(
    message_type: Option<MessageType>,
    choices: &[String],
    fields: &MessageFieldOptions,
) -> Result<(), CommandError> {
    match message_type {
        Some(MessageType::Fyi) => {
            if !choices.is_empty() {
                return Err(CommandError::new(
                    "--type fyi carries only the Seen acknowledgment: remove --choice, or use --type request",
                    EXIT_REFUSED,
                ));
            }
            if fields.reply {
                return Err(CommandError::new(
                    "--type fyi carries only the Seen acknowledgment: remove --reply, or use --type request",
                    EXIT_REFUSED,
                ));
            }
            if fields.default_choice.is_some() || fields.default_at.is_some() {
                return Err(CommandError::new(
                    "--type fyi carries only the Seen acknowledgment: remove --default and --default-at, or use --type request",
                    EXIT_REFUSED,
                ));
            }
            if !fields.choice_styles.is_empty() {
                return Err(CommandError::new(
                    "--type fyi carries only the Seen acknowledgment: remove --choice-style, or use --type request",
                    EXIT_REFUSED,
                ));
            }
        }
        Some(MessageType::Request) => {
            for (index, choice) in choices.iter().enumerate() {
                if choice.trim().is_empty() {
                    return Err(CommandError::new(
                        "--choice must have a non-empty label; provide a label or remove --choice",
                        EXIT_REFUSED,
                    ));
                }
                if choices[..index].iter().any(|prior| prior == choice) {
                    return Err(CommandError::new(
                        format!(
                            "--choice {choice:?} is repeated; remove the duplicate or use a distinct label"
                        ),
                        EXIT_REFUSED,
                    ));
                }
            }
            if choices.is_empty() && !fields.reply {
                return Err(CommandError::new(
                    "--type request needs at least one choice or a reply: add --choice <label> or --reply, or send as --type fyi",
                    EXIT_REFUSED,
                ));
            }
        }
        None => {}
    }
    Ok(())
}

fn validate_revision_controls(
    message_type: Option<MessageType>,
    choices: &[String],
    clear_choices: bool,
    fields: &MessageFieldOptions,
) -> Result<(), CommandError> {
    if message_type == Some(MessageType::Fyi) {
        if !choices.is_empty() {
            return Err(CommandError::new(
                "--type fyi carries only the Seen acknowledgment: remove --choice, or revise as --type request",
                EXIT_REFUSED,
            ));
        }
        if clear_choices {
            return Err(CommandError::new(
                "--type fyi carries only the Seen acknowledgment: remove --clear-choices, or revise as --type request",
                EXIT_REFUSED,
            ));
        }
        if fields.reply {
            return Err(CommandError::new(
                "--type fyi carries only the Seen acknowledgment: remove --reply, or revise as --type request",
                EXIT_REFUSED,
            ));
        }
        if fields.default_choice.is_some() || fields.default_at.is_some() || fields.clear_default {
            return Err(CommandError::new(
                "--type fyi carries only the Seen acknowledgment: remove --default, --default-at and --clear-default, or revise as --type request",
                EXIT_REFUSED,
            ));
        }
        if !fields.choice_styles.is_empty() {
            return Err(CommandError::new(
                "--type fyi carries only the Seen acknowledgment: remove --choice-style, or revise as --type request",
                EXIT_REFUSED,
            ));
        }
    }
    if message_type == Some(MessageType::Request) && clear_choices && !fields.reply {
        return Err(CommandError::new(
            "--type request needs at least one choice or a reply: add --choice <label> or --reply, or revise as --type fyi",
            EXIT_REFUSED,
        ));
    }
    Ok(())
}

fn first_field_error(normalized: &NormalizedMessageFields) -> Result<(), CommandError> {
    if let Some(error) = normalized.errors.first() {
        return Err(CommandError::new(error.clone(), EXIT_REFUSED));
    }
    Ok(())
}

fn validate_send_options(
    choices: &[String],
    fields: &MessageFieldOptions,
    include_thread: bool,
) -> Result<(Option<MessageType>, NormalizedMessageFields), CommandError> {
    validate_answer_ui_inputs(choices, fields)?;
    let message_type = Some(send_message_type(fields)?);
    if let Some(report) = &fields.progress {
        report.validate()?;
    }
    if fields.progress.is_some() && message_type != Some(MessageType::Fyi) {
        return Err(CommandError::new(
            "a Progress message is an FYI: use --progress with --type fyi or leave --type out",
            EXIT_REFUSED,
        ));
    }
    validate_message_kind(fields)?;
    if fields.answer_ui.is_none() {
        validate_send_controls(message_type, choices, fields)?;
        let answer_ui = answer_ui_for_send(choices, message_type, fields)?;
        validate_typed_answer_ui(message_type, &answer_ui)?;
    }
    let normalized =
        normalize_message_fields(fields, &send_choice_targets(choices), include_thread);
    first_field_error(&normalized)?;
    Ok((message_type, normalized))
}

fn validate_answer_ui_inputs(
    choices: &[String],
    fields: &MessageFieldOptions,
) -> Result<(), CommandError> {
    let Some(answer_ui) = fields.answer_ui.as_ref() else {
        return Ok(());
    };

    if !answer_ui.is_object() || answer_ui["schema"] != SENDER_ANSWER_UI_SCHEMA {
        return Err(CommandError::new(
            format!(
                "--answer-ui must be a JSON object with schema \"{SENDER_ANSWER_UI_SCHEMA}\"; fix the file, or drop --answer-ui and use --choice/--reply"
            ),
            EXIT_REFUSED,
        ));
    }

    if fields.message_type.as_deref() == Some("fyi") {
        return Err(CommandError::new(
            "--answer-ui requires --type request: replace --type fyi with --type request, or drop --answer-ui and use --type fyi",
            EXIT_REFUSED,
        ));
    }

    match fields.message_type.as_deref() {
        Some("request") => {}
        Some(other) => {
            return Err(CommandError::new(
                format!(
                    "--answer-ui requires --type request: replace --type {other:?} with --type request, or drop --answer-ui and use --choice/--reply"
                ),
                EXIT_REFUSED,
            ));
        }
        None => {
            return Err(CommandError::new(
                "--answer-ui requires explicit --type request: add --type request, or drop --answer-ui and use --choice/--reply",
                EXIT_REFUSED,
            ));
        }
    }

    let mut conflicts = Vec::new();
    if !choices.is_empty() {
        conflicts.push("--choice");
    }
    if fields.reply {
        conflicts.push("--reply");
    }
    if fields.default_choice.is_some() {
        conflicts.push("--default");
    }
    if fields.default_at.is_some() {
        conflicts.push("--default-at");
    }
    if !fields.choice_styles.is_empty() {
        conflicts.push("--choice-style");
    }
    if !conflicts.is_empty() {
        return Err(CommandError::new(
            format!(
                "--answer-ui carries its own inputs: remove {}, or drop --answer-ui and use --choice/--reply",
                conflicts.join(", ")
            ),
            EXIT_REFUSED,
        ));
    }
    Ok(())
}

fn validate_typed_answer_ui(
    message_type: Option<MessageType>,
    answer_ui: &Value,
) -> Result<(), CommandError> {
    match message_type {
        Some(MessageType::Fyi) if *answer_ui != seen_answer_ui() => Err(CommandError::new(
            "--type fyi carries only the Seen acknowledgment; revise with --type fyi to restore Seen, or use --type request with --choice or --reply",
            EXIT_REFUSED,
        )),
        Some(MessageType::Request) => {
            if answer_ui.is_null() {
                return Err(CommandError::new(
                    "--type request needs at least one choice or a reply: add --choice <label> or --reply, or revise as --type fyi",
                    EXIT_REFUSED,
                ));
            }
            if answer_ui["schema"] != ANSWER_UI_SCHEMA {
                return Err(CommandError::new(
                    "--type request supports only attention-client.actions/1 controls; replace them with --choice or --reply",
                    EXIT_REFUSED,
                ));
            }
            let Some(actions) = answer_ui["actions"].as_array() else {
                return Err(CommandError::new(
                    "--type request needs a usable actions/1 control: replace the controls with --choice or --reply",
                    EXIT_REFUSED,
                ));
            };
            if actions.is_empty() || actions.iter().all(|action| action["id"] == "seen") {
                return Err(CommandError::new(
                    "--type request needs at least one choice or a reply: add --choice <label> or --reply, or revise as --type fyi",
                    EXIT_REFUSED,
                ));
            }
            if !valid_typed_answer_ui_descriptor(answer_ui) {
                return Err(CommandError::new(
                    "--type request needs a usable actions/1 control: replace the controls with --choice or --reply",
                    EXIT_REFUSED,
                ));
            }
            if actions.iter().any(|action| action["id"] == "seen") {
                return Err(CommandError::new(
                    "--type request cannot carry the Seen action: replace the controls with --choice or --reply, or revise as --type fyi",
                    EXIT_REFUSED,
                ));
            }
            Ok(())
        }
        Some(MessageType::Fyi) | None => Ok(()),
    }
}

fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let value = value.as_bytes();
    if value.len() < 20 || value.get(10).copied()? != b'T' && value.get(10).copied()? != b't' {
        return None;
    }

    let year = parse_digits(value.get(0..4)?)? as i64;
    let month = parse_digits(value.get(5..7)?)? as i64;
    let day = parse_digits(value.get(8..10)?)? as i64;
    let hour = parse_digits(value.get(11..13)?)? as i64;
    let minute = parse_digits(value.get(14..16)?)? as i64;
    let second = parse_digits(value.get(17..19)?)? as i64;
    if value.get(4).copied()? != b'-'
        || value.get(7).copied()? != b'-'
        || value.get(13).copied()? != b':'
        || value.get(16).copied()? != b':'
        || month == 0
        || month > 12
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut cursor = 19;
    let mut fraction_ms = 0i64;
    if value.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while value.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
        let fraction = value.get(start..cursor)?;
        for index in 0..3 {
            fraction_ms *= 10;
            if let Some(digit) = fraction.get(index) {
                fraction_ms += (digit - b'0') as i64;
            }
        }
    }

    let offset_minutes = match value.get(cursor).copied()? {
        b'Z' | b'z' if cursor + 1 == value.len() => 0i64,
        sign @ (b'+' | b'-') if cursor + 6 == value.len() => {
            if value.get(cursor + 3) != Some(&b':') {
                return None;
            }
            let hours = parse_digits(value.get(cursor + 1..cursor + 3)?)? as i64;
            let minutes = parse_digits(value.get(cursor + 4..cursor + 6)?)? as i64;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let magnitude = hours * 60 + minutes;
            if sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let local_seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?;
    local_seconds
        .checked_sub(offset_minutes * 60)?
        .checked_mul(1_000)?
        .checked_add(fraction_ms)
}

fn parse_digits(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    value.iter().try_fold(0u32, |number, digit| {
        number.checked_mul(10)?.checked_add((digit - b'0') as u32)
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn text_reply_action() -> Value {
    json!({"id":"reply","label":"Reply","kind":"text"})
}

fn text_reply_answer_ui() -> Value {
    json!({"schema":ANSWER_UI_SCHEMA,"actions":[text_reply_action()]})
}

fn seen_answer_ui() -> Value {
    json!({
        "schema":ANSWER_UI_SCHEMA,
        "actions":[{"id":"seen","label":"Seen","kind":"value","value":{"acknowledgment":"seen"}}]
    })
}

fn choices_answer_ui_with_reply(
    choices: &[String],
    include_reply: bool,
) -> Result<Value, CommandError> {
    let mut seen = BTreeSet::new();
    let mut actions = Vec::with_capacity(choices.len() + usize::from(include_reply));
    for (index, choice) in choices.iter().enumerate() {
        if choice.trim().is_empty() {
            return Err(CommandError::new(
                format!("--choice {} is empty", index + 1),
                EXIT_REFUSED,
            ));
        }
        if !seen.insert(choice.as_str()) {
            return Err(CommandError::new(
                format!("--choice {choice:?} is given more than once"),
                EXIT_REFUSED,
            ));
        }
        actions.push(json!({
            "id": format!("choice-{}", index + 1),
            "label": choice,
            "kind": "value",
            "value": choice
        }));
    }
    if include_reply {
        actions.push(text_reply_action());
    }
    Ok(json!({ "schema": ANSWER_UI_SCHEMA, "actions": actions }))
}

fn choices_answer_ui(choices: &[String]) -> Result<Value, CommandError> {
    choices_answer_ui_with_reply(choices, true)
}

fn answer_ui_for_send(
    choices: &[String],
    message_type: Option<MessageType>,
    fields: &MessageFieldOptions,
) -> Result<Value, CommandError> {
    if let Some(answer_ui) = &fields.answer_ui {
        return Ok(answer_ui.clone());
    }
    match message_type {
        Some(MessageType::Fyi) => Ok(seen_answer_ui()),
        Some(MessageType::Request) => choices_answer_ui_with_reply(choices, fields.reply),
        None => choices_answer_ui(choices),
    }
}

fn valid_typed_answer_ui_descriptor(answer_ui: &Value) -> bool {
    if !has_exact_members(answer_ui, &["schema", "actions"])
        || answer_ui["schema"] != ANSWER_UI_SCHEMA
    {
        return false;
    }
    let Some(actions) = answer_ui["actions"].as_array() else {
        return false;
    };
    if actions.is_empty() {
        return false;
    }

    let mut ids = BTreeSet::new();
    actions.iter().all(|action| {
        if !action.is_object() {
            return false;
        }
        let Some(id) = action["id"].as_str() else {
            return false;
        };
        let Some(label) = action["label"].as_str() else {
            return false;
        };
        if id.is_empty() || label.is_empty() || !ids.insert(id) {
            return false;
        }
        match action["kind"].as_str() {
            Some("text") => has_exact_members(action, &["id", "label", "kind"]),
            Some("value") => has_exact_members(action, &["id", "label", "kind", "value"]),
            _ => false,
        }
    })
}

fn valid_answer_ui(answer_ui: &Value) -> bool {
    if answer_ui["schema"] != ANSWER_UI_SCHEMA {
        return false;
    }
    let Some(actions) = answer_ui["actions"].as_array() else {
        return false;
    };
    if actions.is_empty() {
        return false;
    }

    let mut ids = BTreeSet::new();
    actions.iter().all(|action| {
        let Some(id) = action["id"].as_str() else {
            return false;
        };
        let Some(label) = action["label"].as_str() else {
            return false;
        };
        if id.trim().is_empty() || label.trim().is_empty() || !ids.insert(id) {
            return false;
        }
        match action["kind"].as_str() {
            Some("text") => true,
            Some("value") => action.get("value").is_some(),
            _ => false,
        }
    })
}

fn has_exact_members(value: &Value, members: &[&str]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == members.len() && members.iter().all(|member| object.contains_key(*member))
}

fn answer_ui_for_revision(
    current: &Value,
    add_reply: bool,
    strict: bool,
) -> Result<Value, CommandError> {
    if let Some(answer_ui) = current.get("answer_ui") {
        let valid = if strict {
            valid_typed_answer_ui_descriptor(answer_ui)
        } else {
            valid_answer_ui(answer_ui)
        };
        if !valid {
            let message = if strict {
                "current answer controls are unsupported or invalid; replace them with --choice <label>, or use --clear-choices --reply"
            } else {
                "current request has an invalid attention-client.actions/1 descriptor; refusing to revise"
            };
            return Err(CommandError::new(message, EXIT_REFUSED));
        }
        let actions = answer_ui["actions"].as_array().unwrap();
        if actions.len() == 1 && actions[0] == seen_answer_ui()["actions"][0] {
            return Ok(if add_reply && strict {
                text_reply_answer_ui()
            } else {
                answer_ui.clone()
            });
        }

        let mut actions = actions.clone();
        if add_reply && !actions.iter().any(|action| action["kind"] == "text") {
            actions.push(text_reply_action());
        }
        return Ok(json!({"schema":ANSWER_UI_SCHEMA,"actions":actions}));
    }

    Ok(if add_reply {
        text_reply_answer_ui()
    } else {
        Value::Null
    })
}

pub(crate) fn revised_message_with_fields(
    current: &Value,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    clear_choices: bool,
    fields: &MessageFieldOptions,
) -> Result<(Value, Vec<String>), CommandError> {
    if clear_choices && !choices.is_empty() {
        return Err(CommandError::new(
            "use either --choice or --clear-choices, not both",
            EXIT_REFUSED,
        ));
    }
    let message_type = revise_message_type(current, fields)?;
    validate_message_kind(fields)?;
    validate_revision_controls(message_type, choices, clear_choices, fields)?;

    let answer_ui = match message_type {
        Some(MessageType::Fyi) => {
            if fields.message_type.as_deref() == Some("fyi") {
                seen_answer_ui()
            } else {
                answer_ui_for_revision(current, false, true)?
            }
        }
        Some(MessageType::Request) => {
            if clear_choices {
                text_reply_answer_ui()
            } else if !choices.is_empty() {
                choices_answer_ui_with_reply(choices, fields.reply)?
            } else if fields.reply {
                answer_ui_for_revision(current, true, true)?
            } else {
                answer_ui_for_revision(current, false, true)?
            }
        }
        None if clear_choices => text_reply_answer_ui(),
        None if !choices.is_empty() => choices_answer_ui(choices)?,
        None => answer_ui_for_revision(current, true, false)?,
    };
    let replacing_choices = clear_choices || !choices.is_empty();
    let mut message = current.clone();
    message["title"] = Value::String(title.to_owned());
    message["created_at"] = Value::from(now_ms());
    if let Some(body) = body {
        message["body"] = Value::String(body.to_owned());
    }
    if let Some(message_type) = message_type {
        message["message_type"] = Value::String(message_type.as_str().to_owned());
    }
    if let Some(message_kind) = fields.message_kind.as_deref() {
        message["message_kind"] = Value::String(message_kind.to_owned());
    }
    if let Some(object) = message.as_object_mut() {
        object.remove("choices");
        if message_type == Some(MessageType::Fyi) {
            object.remove("default");
            object.remove("choice_styles");
        }
    }
    message["answer_ui"] = answer_ui;
    validate_typed_answer_ui(message_type, &message["answer_ui"])?;
    if replacing_choices {
        if let Some(object) = message.as_object_mut() {
            object.remove("default");
            object.remove("choice_styles");
        }
    }

    let mut options = fields.clone();
    let mut warnings = Vec::new();
    if options.clear_default && (options.default_choice.is_some() || options.default_at.is_some()) {
        warnings
            .push("--default conflicts with --clear-default; leaving the default cleared".into());
        options.default_choice = None;
        options.default_at = None;
    }
    if options.clear_default {
        if let Some(object) = message.as_object_mut() {
            object.remove("default");
        }
    }

    let normalized = normalize_message_fields(
        &options,
        &answer_ui_choice_targets(&message["answer_ui"]),
        false,
    );
    warnings.extend(normalized.warnings.iter().cloned());
    first_field_error(&normalized)?;
    let normalized_members = normalized.members;
    for (field, value) in &normalized_members {
        message[field] = value.clone();
    }

    // A valid member supplied on a revise replaces its prior value. An invalid
    // member is treated as absent, preserving the prior revision's value.
    for field in ["stake"] {
        if let Some(raw) = options_field(&options, field) {
            if !normalized_member_is_valid(field, raw) {
                if let Some(old_value) = current.get(field) {
                    message[field] = old_value.clone();
                }
            }
        }
    }
    if (options.default_choice.is_some() || options.default_at.is_some())
        && !normalized_members.contains_key("default")
        && !replacing_choices
    {
        if let Some(old_value) = current.get("default") {
            message["default"] = old_value.clone();
        }
    }
    if !options.choice_styles.is_empty()
        && !normalized_members.contains_key("choice_styles")
        && !replacing_choices
    {
        if let Some(old_value) = current.get("choice_styles") {
            message["choice_styles"] = old_value.clone();
        }
    }
    Ok((message, warnings))
}

pub(crate) fn progress_content(
    current: &Value,
    report: &ProgressReport,
    at_ms: u64,
) -> Result<Value, CommandError> {
    report.validate()?;
    if current["message_type"] != "fyi" || current.get("progress").is_none() {
        return Err(CommandError::new(
            "only a Progress FYI can be updated",
            EXIT_REFUSED,
        ));
    }
    let mut message = current.clone();
    message["progress"] = progress::next_progress(current, report, at_ms)?;
    message["created_at"] = at_ms.into();
    Ok(message)
}

fn options_field<'a>(options: &'a MessageFieldOptions, field: &str) -> Option<&'a str> {
    match field {
        "urgency" => options.importance.as_deref(),
        "by_when" => options.by_when.as_deref(),
        "stake" => options.stake.as_deref(),
        _ => None,
    }
}

fn normalized_member_is_valid(field: &str, value: &str) -> bool {
    match field {
        "urgency" => matches!(value, "low" | "normal" | "high" | "now"),
        "by_when" => parse_rfc3339_ms(value).is_some(),
        "stake" => !value.is_empty(),
        _ => false,
    }
}

pub(crate) fn build_send_content(
    title: &str,
    body: Option<&str>,
    choices: &[String],
    fields: &MessageFieldOptions,
    created_at: u64,
) -> Result<Value, CommandError> {
    let (message_type, normalized) = validate_send_options(choices, fields, true)?;
    let answer_ui = answer_ui_for_send(choices, message_type, fields)?;
    let mut message = json!({
        "created_at": created_at,
        "title": title
    });
    if let Some(report) = &fields.progress {
        message["progress"] = progress::initial_progress(report, created_at);
    }
    if let Some(body) = body {
        message["body"] = Value::String(body.to_owned());
    }
    if let Some(message_type) = message_type {
        message["message_type"] = Value::String(message_type.as_str().to_owned());
    }
    if let Some(message_kind) = fields.message_kind.as_deref() {
        message["message_kind"] = Value::String(message_kind.to_owned());
    }
    message["answer_ui"] = answer_ui;
    if let Some(display_name) = &fields.display_name {
        message["display_name"] = Value::String(display_name.clone());
    }
    for (field, value) in normalized.members {
        message[field] = value;
    }

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_become_value_actions_in_the_order_given() {
        let choices = vec!["Approve".to_string(), "Hold".to_string()];
        assert_eq!(
            choices_answer_ui(&choices).unwrap(),
            json!({
                "schema":"attention-client.actions/1",
                "actions":[
                    {"id":"choice-1","label":"Approve","kind":"value","value":"Approve"},
                    {"id":"choice-2","label":"Hold","kind":"value","value":"Hold"},
                    {"id":"reply","label":"Reply","kind":"text"}
                ]
            })
        );
        let descriptor = choices_answer_ui(&["Hold until Monday".to_string()]).unwrap();
        let action = &descriptor["actions"][0];
        assert_eq!(action["label"], action["value"]);
        assert_eq!(action.as_object().unwrap().len(), 4);
        assert_eq!(descriptor["actions"][1], text_reply_action());
    }

    #[test]
    fn rfc3339_message_times_normalize_to_epoch_milliseconds() {
        assert_eq!(
            parse_rfc3339_ms("2026-10-30T15:00:00.123-07:00"),
            parse_rfc3339_ms("2026-10-30T22:00:00.123Z")
        );
        assert!(parse_rfc3339_ms("2024-02-29T00:00:00Z").is_some());
        assert!(parse_rfc3339_ms("2026-02-29T00:00:00Z").is_none());
        assert!(parse_rfc3339_ms("2026-10-30T15:00:00+24:00").is_none());
        assert!(parse_rfc3339_ms("tomorrow").is_none());
    }

    #[test]
    fn invalid_importance_and_by_when_are_errors_while_other_optional_fields_warn() {
        let choices = vec!["Hold".to_string(), "Ship".to_string()];
        let options = MessageFieldOptions {
            importance: Some("urgent".into()),
            by_when: Some("tomorrow".into()),
            stake: Some("".into()),
            thread: Some("".into()),
            display_name: None,
            default_choice: Some("Later".into()),
            default_at: Some("bad-time".into()),
            choice_styles: vec!["Later=primary".into(), "Ship=urgent".into()],
            clear_default: false,
            ..MessageFieldOptions::default()
        };

        let normalized = normalize_message_fields(&options, &send_choice_targets(&choices), true);

        assert!(normalized.members.is_empty());
        assert_eq!(
            normalized.errors,
            vec![
                "--importance must be one of low, normal, high, now; got \"urgent\"",
                "--by-when must be a valid RFC 3339 time; got \"tomorrow\" (for example, 2026-11-01T17:00:00Z)"
            ]
        );
        assert_eq!(
            normalized.warnings,
            vec![
                "--default choice \"Later\" does not name a choice; leaving the default out",
                "--default-at value \"bad-time\" is not a valid RFC 3339 time; leaving the default out",
                "--thread is empty; leaving thread out",
                "--choice-style choice \"Later\" does not name a choice; leaving this style out",
                "--choice-style value \"Ship=urgent\" has an unsupported style; leaving this style out",
                "--stake is empty; leaving stake out",
            ]
        );
    }

    #[test]
    fn typed_revise_preserves_kind_and_controls_without_adding_unrequested_reply() {
        let current = json!({
            "title":"Old title",
            "message_type":"request",
            "message_kind":"blocker",
            "default":{"value":"Approve","at":9999999999999_i64},
            "choice_styles":{"choice-1":"primary"},
            "answer_ui":choices_answer_ui_with_reply(&["Approve".into()], false).unwrap()
        });

        let kept = revised_message_with_fields(
            &current,
            "Retitled",
            None,
            &[],
            false,
            &MessageFieldOptions::default(),
        )
        .unwrap()
        .0;
        assert_eq!(kept["message_type"], "request");
        assert_eq!(kept["message_kind"], "blocker");
        assert_eq!(kept["answer_ui"]["actions"].as_array().unwrap().len(), 1);
        assert_eq!(kept["answer_ui"]["actions"][0]["id"], "choice-1");

        let with_reply = revised_message_with_fields(
            &current,
            "Retitled with Reply",
            None,
            &[],
            false,
            &MessageFieldOptions {
                reply: true,
                ..MessageFieldOptions::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(
            with_reply["answer_ui"]["actions"].as_array().unwrap().len(),
            2
        );
        assert_eq!(with_reply["answer_ui"]["actions"][1]["id"], "reply");

        let replacement = revised_message_with_fields(
            &current,
            "New choice",
            None,
            &["Hold".into()],
            false,
            &MessageFieldOptions::default(),
        )
        .unwrap()
        .0;
        assert_eq!(
            replacement["answer_ui"]["actions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(replacement["answer_ui"]["actions"][0]["id"], "choice-1");

        let fyi = revised_message_with_fields(
            &current,
            "Information only",
            None,
            &[],
            false,
            &MessageFieldOptions {
                message_type: Some("fyi".into()),
                ..MessageFieldOptions::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(fyi["message_type"], "fyi");
        assert_eq!(fyi["message_kind"], "blocker");
        assert_eq!(fyi["answer_ui"], seen_answer_ui());
        assert!(fyi.get("default").is_none());
        assert!(fyi.get("choice_styles").is_none());

        let back_to_request = revised_message_with_fields(
            &fyi,
            "Need a response",
            None,
            &[],
            false,
            &MessageFieldOptions {
                message_type: Some("request".into()),
                reply: true,
                ..MessageFieldOptions::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(back_to_request["message_type"], "request");
        assert_eq!(back_to_request["message_kind"], "blocker");
        assert_eq!(back_to_request["answer_ui"], text_reply_answer_ui());
    }
}
