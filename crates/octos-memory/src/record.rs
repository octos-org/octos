//! Records: the unit the Recall/Knowledge index ranks (ADR "personal memory —
//! three tiers by trust, one index").
//!
//! A record is deliberately small — an id, a title, a short abstract and
//! metadata — because the index stores no bodies: the apps (mail, calendar),
//! the episode store and the markdown bank remain the record of truth and
//! `memory_load` goes back to them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Longest title kept, in bytes (UTF-8 boundary respected).
pub const MAX_TITLE_BYTES: usize = 120;
/// Longest abstract kept, in bytes.
pub const MAX_ABSTRACT_BYTES: usize = 300;
/// Longest optional body kept (opt-in; apps normally send none).
pub const MAX_BODY_BYTES: usize = 16 * 1024;

const CURRENT_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// Which tier a record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// A task/conversation summary mirrored from the episode store.
    Episode,
    /// An app record (mail, calendar event, contact, note…): untrusted.
    Document,
    /// A curated memory-bank page: trusted, human-editable.
    Knowledge,
}

impl RecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::Episode => "episode",
            RecordKind::Document => "document",
            RecordKind::Knowledge => "knowledge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "episode" | "episodes" => Some(RecordKind::Episode),
            "document" | "documents" | "doc" | "docs" => Some(RecordKind::Document),
            "knowledge" | "bank" | "page" | "pages" => Some(RecordKind::Knowledge),
            _ => None,
        }
    }
}

/// How much the content may be trusted when it reaches the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Third-party or app content: data, never instructions.
    #[default]
    Untrusted,
    /// Curated by the user or the consolidation pass.
    Trusted,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Untrusted => "untrusted",
            Trust::Trusted => "trusted",
        }
    }
}

/// One indexed record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Namespaced id: `doc:<source>:<key>`, `bank:<slug>`, `episode:<id>`.
    pub id: String,
    pub kind: RecordKind,
    /// Producer: `mail`, `calendar`, `contacts`, `bank`, `episodes`, …
    pub source: String,
    /// Grouping key (thread id, calendar series, bank section).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// When the underlying thing happened (message date, event start, page
    /// modification time). Drives the hot window and heat.
    pub timestamp: DateTime<Utc>,
    pub title: String,
    #[serde(rename = "abstract")]
    pub abstract_: String,
    /// Optional body, opt-in; the index keeps at most [`MAX_BODY_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub trust: Trust,
    /// Producer-side change detector (hash/mtime/seq): unchanged records are
    /// skipped on re-ingest so vectors are not recomputed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    /// Times this record was loaded by the model or the user.
    #[serde(default)]
    pub visits: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_visit: Option<DateTime<Utc>>,
    /// Set once the record has been nominated into the Knowledge staging
    /// area; promotion never repeats.
    #[serde(default)]
    pub promoted: bool,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

impl Record {
    pub fn new(
        id: impl Into<String>,
        kind: RecordKind,
        source: impl Into<String>,
        timestamp: DateTime<Utc>,
        title: impl Into<String>,
        abstract_: impl Into<String>,
    ) -> Self {
        let mut r = Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: id.into(),
            kind,
            source: source.into(),
            parent: None,
            timestamp,
            title: title.into(),
            abstract_: abstract_.into(),
            body: None,
            trust: match kind {
                RecordKind::Knowledge => Trust::Trusted,
                _ => Trust::Untrusted,
            },
            fingerprint: String::new(),
            visits: 0,
            last_visit: None,
            promoted: false,
            updated_at: Utc::now(),
        };
        r.clamp();
        r
    }

    /// Text the index tokenises and embeds: title + abstract (+ parent).
    pub fn index_text(&self) -> String {
        let mut t = String::with_capacity(self.title.len() + self.abstract_.len() + 16);
        t.push_str(&self.title);
        t.push('\n');
        t.push_str(&self.abstract_);
        if let Some(p) = &self.parent {
            t.push('\n');
            t.push_str(p);
        }
        t
    }

    /// Enforce the size caps (UTF-8 safe).
    pub fn clamp(&mut self) {
        self.title = octos_core::truncated_utf8(self.title.trim(), MAX_TITLE_BYTES, "");
        self.abstract_ = octos_core::truncated_utf8(self.abstract_.trim(), MAX_ABSTRACT_BYTES, "");
        if let Some(body) = &self.body {
            if body.trim().is_empty() {
                self.body = None;
            } else if body.len() > MAX_BODY_BYTES {
                self.body = Some(octos_core::truncated_utf8(body, MAX_BODY_BYTES, ""));
            }
        }
    }

    /// Approximate byte size the record occupies (for the heat's size term).
    pub fn size_bytes(&self) -> usize {
        self.title.len() + self.abstract_.len() + self.body.as_ref().map_or(0, String::len)
    }

    /// MemoryOS-style heat: visits + size + recency, with recency decaying
    /// exponentially over `half_life_days`. Higher is hotter. Used to pick
    /// which records keep a vector, which are evicted, and which are
    /// nominated for promotion.
    pub fn heat(&self, now: DateTime<Utc>, half_life_days: f32) -> f32 {
        let anchor = self
            .last_visit
            .unwrap_or(self.timestamp)
            .max(self.timestamp);
        let age_days = (now - anchor).num_seconds().max(0) as f32 / 86_400.0;
        let recency = (-(age_days * std::f32::consts::LN_2) / half_life_days.max(1.0)).exp();
        let size = (self.size_bytes() as f32 / MAX_ABSTRACT_BYTES as f32).min(1.0);
        self.visits as f32 + size + recency
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn should_clamp_title_and_abstract_on_utf8_boundaries() {
        let long = "日".repeat(200);
        let r = Record::new(
            "doc:mail:1",
            RecordKind::Document,
            "mail",
            Utc::now(),
            &long,
            &long,
        );
        assert!(r.title.len() <= MAX_TITLE_BYTES);
        assert!(r.abstract_.len() <= MAX_ABSTRACT_BYTES);
        assert!(std::str::from_utf8(r.title.as_bytes()).is_ok());
        assert_eq!(r.trust, Trust::Untrusted);
        assert_eq!(
            Record::new(
                "bank:x",
                RecordKind::Knowledge,
                "bank",
                Utc::now(),
                "x",
                "y"
            )
            .trust,
            Trust::Trusted
        );
    }

    #[test]
    fn should_cool_with_age_and_warm_with_visits() {
        let now = Utc::now();
        let fresh = Record::new("a", RecordKind::Document, "mail", now, "t", "a");
        let mut old = Record::new(
            "b",
            RecordKind::Document,
            "mail",
            now - Duration::days(400),
            "t",
            "a",
        );
        assert!(fresh.heat(now, 120.0) > old.heat(now, 120.0));
        old.visits = 3;
        assert!(
            old.heat(now, 120.0) > fresh.heat(now, 120.0),
            "visits dominate recency"
        );
    }

    #[test]
    fn should_round_trip_serde_with_abstract_rename() {
        let r = Record::new(
            "doc:cal:1",
            RecordKind::Document,
            "calendar",
            Utc::now(),
            "Dentist",
            "10:30 at Sunrise Dental",
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"abstract\":"));
        let back: Record = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
