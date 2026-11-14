//! Weekday translation at user-input and presentation boundaries.
//!
//! Stored expressions and the scheduler retain cron 0.15's Sunday=1 numbering.
//! New user input uses Sunday=0/7 and is stored with names instead of numbers.

use std::str::FromStr;

use cron::TimeUnitSpec;
use eyre::{Result, WrapErr, ensure};

const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn weekday(point: &str) -> Result<usize> {
    let value = match point.to_ascii_lowercase().as_str() {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tues" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thurs" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => {
            ensure!(
                !point.is_empty() && point.bytes().all(|b| b.is_ascii_digit()),
                "weekday must be 0-7 or SUN-SAT"
            );
            point.parse::<usize>().wrap_err("invalid weekday number")?
        }
    };
    ensure!(value <= 7, "weekday must be 0-7 or SUN-SAT");
    Ok(value)
}

fn named_days(selected: &[bool; 7]) -> String {
    if selected.iter().all(|selected| *selected) {
        return "*".into();
    }
    DAYS.iter()
        .zip(selected)
        .filter_map(|(name, selected)| selected.then_some(*name))
        .collect::<Vec<_>>()
        .join(",")
}

// Whitespace around list/range/step operators belongs to the same field.
// Keep that text intact so the cron parser still validates non-weekday syntax.
fn cron_fields(expr: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let (mut start, mut end) = (0, 0);
    let mut previous = None;
    for (index, ch) in expr.char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        if previous.is_none() {
            start = index;
        } else if end < index
            && !matches!(previous, Some(',' | '-' | '/'))
            && !matches!(ch, ',' | '-' | '/')
        {
            fields.push(&expr[start..end]);
            start = index;
        }
        previous = Some(ch);
        end = index + ch.len_utf8();
    }
    if previous.is_some() {
        fields.push(&expr[start..end]);
    }
    fields
}

/// Translate a user expression's weekday field to names. Ranges and steps are
/// expanded in the 0..=7 input domain BEFORE the two Sunday aliases are merged.
/// Other fields keep the existing cron-crate grammar and calendar semantics.
pub fn standard_cron_expression(expr: &str) -> Result<String> {
    let mut fields = cron_fields(expr);
    if fields.len() == 1 && expr.trim().starts_with('@') {
        cron::Schedule::from_str(expr).wrap_err("invalid cron expression")?;
        return Ok(expr.to_owned());
    }
    ensure!(
        matches!(fields.len(), 6 | 7),
        "cron expression needs seconds, minutes, hours, day, month, weekday (and optional year)"
    );
    let mut selected = [false; 7];
    let weekday_field: String = fields[5].chars().filter(|ch| !ch.is_whitespace()).collect();
    for term in weekday_field.split(',') {
        let (base, step) = match term.split_once('/') {
            Some((base, step)) => {
                ensure!(
                    !step.is_empty() && step.bytes().all(|b| b.is_ascii_digit()),
                    "weekday step must be a positive integer"
                );
                let step = step.parse::<usize>().wrap_err("invalid weekday step")?;
                ensure!(step > 0, "weekday step must be positive");
                (base, Some(step))
            }
            None => (term, None),
        };
        let (start, end) = if matches!(base, "*" | "?") {
            (0, 7)
        } else if let Some((start, end)) = base.split_once('-') {
            (weekday(start)?, weekday(end)?)
        } else {
            let start = weekday(base)?;
            (start, if step.is_some() { 7 } else { start })
        };
        ensure!(start <= end, "weekday range must be ascending");
        for day in (start..=end).step_by(step.unwrap_or(1)) {
            selected[day % 7] = true;
        }
    }
    let names = named_days(&selected);
    fields[5] = &names;
    let normalized = fields.join(" ");
    cron::Schedule::from_str(&normalized).wrap_err("invalid cron expression")?;
    Ok(normalized)
}

fn legacy_named_expression(expr: &str) -> Result<String> {
    let schedule = cron::Schedule::from_str(expr).wrap_err("invalid stored cron expression")?;
    if expr.trim().starts_with('@') {
        return Ok(expr.to_owned());
    }
    // The storage parser permits whitespace inside lists, ranges and steps.
    // Rebuild from parsed fields instead of mistaking those spaces for columns.
    fn field(spec: &impl TimeUnitSpec) -> String {
        if spec.is_all() {
            "*".into()
        } else {
            spec.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        }
    }
    let mut selected = [false; 7];
    for day in schedule.days_of_week().iter() {
        selected[(day - 1) as usize] = true;
    }
    let mut fields = vec![
        field(schedule.seconds()),
        field(schedule.minutes()),
        field(schedule.hours()),
        field(schedule.days_of_month()),
        field(schedule.months()),
        named_days(&selected),
    ];
    // Unrestricted years need no seventh field; explicit year restrictions
    // must survive display/copy without creating a year from a list item.
    if !schedule.years().is_all() {
        fields.push(field(schedule.years()));
    }
    Ok(fields.join(" "))
}

/// Render the historical storage grammar without ambiguous weekday numbers.
/// Invalid stored input is visibly labelled; rendering never rewrites the job.
pub fn display_cron_expression(expr: &str) -> String {
    match legacy_named_expression(expr) {
        Ok(expr) => expr,
        Err(error) => {
            tracing::warn!(%error, "cannot render stored cron expression");
            format!("invalid cron expression: {expr}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CronSchedule, CronService};
    use chrono::{Datelike, TimeZone, Timelike};

    // UPCR-058: user weekdays must survive input normalization and timezone
    // scheduling, while the on-disk numbering remains a separate contract.
    #[test]
    fn standard_weekdays_schedule_in_local_time() {
        for zone in [
            chrono_tz::UTC,
            chrono_tz::America::New_York,
            chrono_tz::Europe::London,
        ] {
            let after = zone.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
            for (field, expected) in [
                ("0,7,SUN", vec![0]),
                ("1-5", vec![1, 2, 3, 4, 5]),
                ("mon-fri", vec![1, 2, 3, 4, 5]),
                ("1-5/2", vec![1, 3, 5]),
                ("MON,WED,FRI", vec![1, 3, 5]),
                ("MON-FRI/2", vec![1, 3, 5]),
                ("5-7", vec![0, 5, 6]),
                ("*/2", vec![0, 2, 4, 6]),
                ("1/2", vec![0, 1, 3, 5]),
            ] {
                let normalized = standard_cron_expression(&format!("0 0 9 * * {field}")).unwrap();
                let dates: Vec<_> = cron::Schedule::from_str(&normalized)
                    .unwrap()
                    .after(&after)
                    .take(28)
                    .collect();
                assert_eq!(dates.len(), 28);
                let mut actual: Vec<_> = dates
                    .iter()
                    .map(|d| d.weekday().num_days_from_sunday())
                    .collect();
                actual.sort_unstable();
                actual.dedup();
                assert_eq!(actual, expected, "{field} {zone}");
                assert!(
                    dates
                        .iter()
                        .all(|d| (d.hour(), d.minute(), d.second()) == (9, 0, 0))
                );
            }
        }
        for (input, expected) in [
            ("0 0, 30 8 - 9 * JAN, MAR MON", "0 0,30 8-9 * JAN,MAR MON"),
            (
                "0 0,\t30 8 - 9 / 1 * JAN - MAR 0,\n7 2098 - 2099",
                "0 0,30 8-9/1 * JAN-MAR SUN 2098-2099",
            ),
            ("0 0 9 * * 1 - 5 / 2", "0 0 9 * * MON,WED,FRI"),
        ] {
            let after = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
            let actual = standard_cron_expression(input).unwrap();
            assert_eq!(
                cron::Schedule::from_str(&actual)
                    .unwrap()
                    .after(&after)
                    .take(32)
                    .collect::<Vec<_>>(),
                cron::Schedule::from_str(expected)
                    .unwrap()
                    .after(&after)
                    .take(32)
                    .collect::<Vec<_>>(),
                "{input}"
            );
        }
        // Spaces must not make invalid non-weekday syntax valid.
        for input in [
            "0 0 9 * JAN / 2 MON",
            "0 0 9 * JAN,, MAR MON",
            "0 0 8 - * * * MON",
        ] {
            assert!(cron::Schedule::from_str(input).is_err());
            assert!(standard_cron_expression(input).is_err(), "{input}");
        }
        for field in [
            "8",
            "-1",
            "1-0",
            "MON-SUN",
            "*/0",
            "1//2",
            "1,,2",
            "7SUN",
            "1/99999999999999999999",
        ] {
            assert!(
                standard_cron_expression(&format!("0 0 9 * * {field}")).is_err(),
                "{field}"
            );
        }
    }

    // Never reinterpret a persisted v1 number as new user input: loading,
    // enabling and copying an old Saturday must all keep Saturday (UPCR-058).
    #[tokio::test]
    async fn legacy_weekdays_survive_reload_toggle_and_copy() {
        for (stored, displayed) in [
            ("0 0 9 * * 7", "0 0 9 * * SAT"),
            ("0 0 9 * * 1, 5", "0 0 9 * * SUN,THU"),
            ("0 0 9 * * 1 - 5", "0 0 9 * * SUN,MON,TUE,WED,THU"),
            ("0 0 9 * * 1 - 5 / 2", "0 0 9 * * SUN,TUE,THU"),
            ("0 0 9 * * 1, 5 2099", "0 0 9 * * SUN,THU 2099"),
            (
                "0 0 9 * * 1 - 5 2098 - 2099",
                "0 0 9 * * SUN,MON,TUE,WED,THU 2098,2099",
            ),
            ("0 0, 30 8 - 9 * JAN, MAR 1, 5", "0 0,30 8,9 * 1,3 SUN,THU"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("cron.json");
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let service = std::sync::Arc::new(CronService::new(&path, tx.clone()));
            let old_schedule = CronSchedule::Cron {
                expr: stored.into(),
            };
            let job = service
                .add_job(
                    "legacy".into(),
                    old_schedule,
                    crate::CronPayload {
                        message: "legacy".into(),
                        deliver: false,
                        channel: None,
                        chat_id: None,
                        mode: Default::default(),
                    },
                )
                .unwrap();
            service.stop().await;
            let original = std::fs::read(&path).unwrap();
            let service = std::sync::Arc::new(CronService::new(&path, tx));
            let old = service.list_all_jobs().remove(0);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "read must not migrate"
            );
            let CronSchedule::Cron { expr } = old.schedule.for_display() else {
                panic!("cron")
            };
            assert_eq!(expr, displayed, "{stored}");
            let after = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
            assert_eq!(
                cron::Schedule::from_str(stored)
                    .unwrap()
                    .after(&after)
                    .take(32)
                    .collect::<Vec<_>>(),
                cron::Schedule::from_str(&expr)
                    .unwrap()
                    .after(&after)
                    .take(32)
                    .collect::<Vec<_>>()
            );
            let enabled = service
                .toggle_job_reconciling(&job.id, true)
                .unwrap()
                .unwrap();
            let next =
                chrono::DateTime::from_timestamp_millis(enabled.state.next_run_at_ms.unwrap())
                    .unwrap();
            assert!(
                cron::Schedule::from_str(stored)
                    .unwrap()
                    .days_of_week()
                    .includes(next.weekday().number_from_sunday())
            );
            let disk: crate::cron_types::CronStore =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(
                matches!(&disk.jobs[0].schedule, CronSchedule::Cron { expr } if expr == stored)
            );
            let copied = service
                .add_job(
                    "copy".into(),
                    CronSchedule::from_standard_cron(&expr).unwrap(),
                    old.payload,
                )
                .unwrap();
            assert_eq!(copied.state.next_run_at_ms, enabled.state.next_run_at_ms);
            service.stop().await;
        }
    }
}
