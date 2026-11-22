//! Build and validate the ordered segments of a Progress FYI.
use super::{CommandError, EXIT_REFUSED};
use serde_json::{Value, json};

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
    use super::*;

    #[test]
    fn progress_rejects_inconsistent_carried_widths() {
        let current = json!({"progress":{"percent":40,"segments":[{"stage":"Build","width":30}]}});
        let report = ProgressReport {
            percent: 50,
            stage: "Verify".into(),
            note: None,
        };
        assert!(
            next_progress(&current, &report, 1000)
                .unwrap_err()
                .message
                .contains("do not add up")
        );
    }

    #[test]
    fn repeated_stages_and_lower_percent_preserve_prior_visits() {
        let report = |percent, stage: &str| ProgressReport {
            percent,
            stage: stage.into(),
            note: None,
        };
        let first = initial_progress(&report(40, "Build"), 1000);
        let second =
            next_progress(&json!({"progress":first}), &report(40, "Verify"), 2000).unwrap();
        let third =
            next_progress(&json!({"progress":second}), &report(20, "Recover"), 3000).unwrap();
        let segments = third["segments"].as_array().unwrap();
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment["width"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![20, 0, 0]
        );
        assert_eq!(segments[1]["stage"], "Verify");
        assert_eq!(third["updated_at"], "1970-01-01T00:00:03.000Z");
    }
}
