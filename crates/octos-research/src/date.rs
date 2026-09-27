//! Dates: parsing provider/page timestamps into ISO 8601 and the `since`
//! control (an ISO date/datetime or a relative span like `24h` / `7d`).

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// Parse a timestamp as found in feeds, APIs and page metadata.
///
/// Accepts RFC 3339 (`2026-09-26T10:15:00Z`, with offsets), RFC 2822 (RSS
/// `pubDate`), GDELT's compact `20260926T101500Z`, naive datetimes
/// (`2026-09-25T08:00:00`, `2026-09-25 08:00:00`, assumed UTC) and plain
/// dates (`2026-09-26`, midnight UTC). Returns the instant plus whether a
/// time-of-day was present.
pub fn parse_timestamp(raw: &str) -> Option<(DateTime<Utc>, bool)> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some((dt.with_timezone(&Utc), true));
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Some((dt.with_timezone(&Utc), true));
    }
    // RFC 2822 with a named zone chrono rejects (e.g. "GMT" is fine, "UTC"
    // or "EDT" are not always): retry with the zone replaced by +0000.
    if let Some(idx) = s.rfind(' ') {
        let (head, zone) = s.split_at(idx);
        if zone.trim().chars().all(|c| c.is_ascii_alphabetic()) {
            if let Ok(dt) = DateTime::parse_from_rfc2822(&format!("{head} +0000")) {
                return Some((dt.with_timezone(&Utc), true));
            }
        }
    }
    for fmt in [
        "%Y%m%dT%H%M%SZ",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y%m%d%H%M%S",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some((Utc.from_utc_datetime(&naive), true));
        }
    }
    // Offsets without a colon (`+0800`) that RFC 3339 rejects.
    for fmt in ["%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M:%S%.f%z"] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some((dt.with_timezone(&Utc), true));
        }
    }
    let date_part = s.get(..10).unwrap_or(s);
    for fmt in ["%Y-%m-%d", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(date_part, fmt) {
            let naive = d.and_hms_opt(0, 0, 0)?;
            return Some((Utc.from_utc_datetime(&naive), false));
        }
    }
    None
}

/// Normalize a timestamp string to ISO 8601: `YYYY-MM-DDTHH:MM:SSZ` when a
/// time is known, `YYYY-MM-DD` for a bare date. `None` if unparseable.
pub fn to_iso(raw: &str) -> Option<String> {
    let (dt, has_time) = parse_timestamp(raw)?;
    Some(format_iso(dt, has_time))
}

pub fn format_iso(dt: DateTime<Utc>, has_time: bool) -> String {
    if has_time {
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// The `since` control: only keep results published at/after `cutoff`.
#[derive(Debug, Clone, PartialEq)]
pub struct Since {
    pub cutoff: DateTime<Utc>,
    /// Set when the caller gave a relative span (`24h`, `7d`), so providers
    /// with native relative filters (`when:7d`, `timespan=7d`) get it as-is.
    pub span: Option<Duration>,
}

impl Since {
    /// Parse `24h`, `36 hours`, `7d`, `2w`, `3m` (months, 30d), `1y`, or an
    /// ISO date / datetime. Relative spans are measured back from `now`.
    pub fn parse(raw: &str, now: DateTime<Utc>) -> Result<Self, String> {
        let s = raw.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Err("empty `since`".to_string());
        }
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && digits.len() < s.len() && !s.contains('-') {
            let n: i64 = digits
                .parse()
                .map_err(|_| format!("invalid `since`: {raw}"))?;
            let unit = s[digits.len()..].trim();
            let span = match unit {
                "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(n),
                "d" | "day" | "days" => Duration::days(n),
                "w" | "wk" | "week" | "weeks" => Duration::weeks(n),
                "m" | "mo" | "month" | "months" => Duration::days(30 * n),
                "y" | "yr" | "year" | "years" => Duration::days(365 * n),
                _ => {
                    return Err(format!(
                        "invalid `since` unit in {raw:?} (use h, d, w, m or y)"
                    ));
                }
            };
            if n <= 0 {
                return Err(format!("`since` must be positive: {raw}"));
            }
            return Ok(Self {
                cutoff: now - span,
                span: Some(span),
            });
        }
        let (cutoff, _) = parse_timestamp(raw)
            .ok_or_else(|| format!("invalid `since` {raw:?}: use an ISO date or 24h/7d/2w"))?;
        Ok(Self { cutoff, span: None })
    }

    /// Effective span back from `now` (relative span, or derived from the
    /// absolute cutoff).
    pub fn effective_span(&self, now: DateTime<Utc>) -> Duration {
        self.span.unwrap_or_else(|| now - self.cutoff)
    }

    /// Whether an ISO/feed timestamp is on or after the cutoff. Unknown or
    /// unparseable dates are kept (`true`): we cannot prove they are old.
    pub fn admits(&self, published: Option<&str>) -> bool {
        match published.and_then(parse_timestamp) {
            Some((dt, has_time)) => {
                if has_time {
                    dt >= self.cutoff
                } else {
                    // A bare date covers the whole day.
                    dt + Duration::days(1) > self.cutoff
                }
            }
            None => true,
        }
    }

    /// Coarse bucket used by providers that only take day/week/month/year.
    pub fn bucket(&self, now: DateTime<Utc>) -> TimeBucket {
        let span = self.effective_span(now);
        if span <= Duration::days(1) {
            TimeBucket::Day
        } else if span <= Duration::days(7) {
            TimeBucket::Week
        } else if span <= Duration::days(31) {
            TimeBucket::Month
        } else {
            TimeBucket::Year
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBucket {
    Day,
    Week,
    Month,
    Year,
}

impl TimeBucket {
    /// `day` / `week` / `month` / `year` (SearXNG `time_range`, Tavily
    /// `time_range`, Perplexity `search_recency_filter`).
    pub fn as_word(self) -> &'static str {
        match self {
            TimeBucket::Day => "day",
            TimeBucket::Week => "week",
            TimeBucket::Month => "month",
            TimeBucket::Year => "year",
        }
    }

    /// Brave `freshness` (`pd`/`pw`/`pm`/`py`).
    pub fn brave(self) -> &'static str {
        match self {
            TimeBucket::Day => "pd",
            TimeBucket::Week => "pw",
            TimeBucket::Month => "pm",
            TimeBucket::Year => "py",
        }
    }

    /// Google-style `tbs=qdr:` value (Serper).
    pub fn qdr(self) -> &'static str {
        match self {
            TimeBucket::Day => "qdr:d",
            TimeBucket::Week => "qdr:w",
            TimeBucket::Month => "qdr:m",
            TimeBucket::Year => "qdr:y",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn should_parse_provider_timestamp_formats_to_iso() {
        assert_eq!(
            to_iso("Wed, 23 Sep 2026 07:00:00 GMT").as_deref(),
            Some("2026-09-23T07:00:00Z")
        );
        assert_eq!(
            to_iso("20260926T101500Z").as_deref(),
            Some("2026-09-26T10:15:00Z")
        );
        assert_eq!(
            to_iso("2026-09-25T08:00:00").as_deref(),
            Some("2026-09-25T08:00:00Z")
        );
        assert_eq!(
            to_iso("2026-09-25T10:00:00+02:00").as_deref(),
            Some("2026-09-25T08:00:00Z")
        );
        assert_eq!(to_iso("2026-09-24").as_deref(), Some("2026-09-24"));
        assert_eq!(to_iso("not a date"), None);
    }

    #[test]
    fn should_parse_relative_and_absolute_since() {
        let s = Since::parse("24h", now()).unwrap();
        assert_eq!(s.cutoff, now() - Duration::hours(24));
        assert_eq!(s.bucket(now()), TimeBucket::Day);
        let s = Since::parse("7d", now()).unwrap();
        assert_eq!(s.bucket(now()), TimeBucket::Week);
        let s = Since::parse("2w", now()).unwrap();
        assert_eq!(s.bucket(now()), TimeBucket::Month);
        let s = Since::parse("2026-09-01", now()).unwrap();
        assert_eq!(s.span, None);
        assert_eq!(s.cutoff, Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap());
        assert!(Since::parse("7 parsecs", now()).is_err());
        assert!(Since::parse("", now()).is_err());
    }

    #[test]
    fn should_filter_on_published_and_keep_unknown_dates() {
        let s = Since::parse("7d", now()).unwrap();
        assert!(s.admits(Some("2026-09-26T10:00:00Z")));
        assert!(!s.admits(Some("2026-09-01T10:00:00Z")));
        assert!(s.admits(Some("2026-09-20"))); // the cutoff day itself
        assert!(!s.admits(Some("2026-09-19")));
        assert!(s.admits(None));
        assert!(s.admits(Some("garbage")));
    }
}
