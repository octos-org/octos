//! Durable session usage ledger for profile-level token and cost analytics.
//!
//! One row is written for each completed LLM run. The ledger lives under the
//! profile data directory so session totals and provider/model rollups survive
//! browser refreshes, reconnects, and daemon restarts.
//!
//! Schema versioning is explicit on every event. Version 1 stores raw usage
//! events plus profile/session indexes; older rows without `schema_version`
//! are treated as version 1 during reads so a future backfill can import legacy
//! observations idempotently before a migration tightens the schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use uuid::Uuid;

pub const USAGE_LEDGER_FILE: &str = "usage_ledger.redb";
pub const USAGE_EVENT_SCHEMA_VERSION: u32 = 1;

const USAGE_EVENTS_TABLE: TableDefinition<'static, &str, &str> =
    TableDefinition::new("usage_events");
const USAGE_PROFILE_INDEX_TABLE: TableDefinition<'static, &str, &str> =
    TableDefinition::new("usage_profile_index");
const USAGE_SESSION_INDEX_TABLE: TableDefinition<'static, &str, &str> =
    TableDefinition::new("usage_session_index");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCostSource {
    CatalogEstimate,
    ProviderReported,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    #[serde(default = "default_usage_event_schema_version")]
    pub schema_version: u32,
    pub event_id: String,
    pub timestamp: DateTime<Utc>,
    pub profile_id: String,
    pub session_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(default)]
    pub cost_source: UsageCostSource,
    #[serde(default = "default_usage_channel")]
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

impl UsageEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn completed_run(
        profile_id: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        provider: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
        estimated_cost_usd: Option<f64>,
        cost_source: UsageCostSource,
        channel: impl Into<String>,
        attribution: Option<String>,
    ) -> Self {
        Self {
            schema_version: USAGE_EVENT_SCHEMA_VERSION,
            event_id: Uuid::now_v7().to_string(),
            timestamp: Utc::now(),
            profile_id: profile_id.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            provider,
            model,
            base_url,
            input_tokens,
            output_tokens,
            estimated_cost_usd,
            cost_source,
            channel: channel.into(),
            attribution,
        }
    }
}

fn default_usage_event_schema_version() -> u32 {
    USAGE_EVENT_SCHEMA_VERSION
}

fn default_usage_channel() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub run_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: f64,
}

impl UsageTotals {
    pub fn add_event(&mut self, event: &UsageEvent) {
        self.run_count = self.run_count.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(event.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(event.output_tokens);
        if let Some(cost) = event.estimated_cost_usd {
            self.estimated_cost_usd += cost;
        }
    }

    pub fn merge(&mut self, other: &UsageTotals) {
        self.run_count = self.run_count.saturating_add(other.run_count);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.estimated_cost_usd += other.estimated_cost_usd;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRollup {
    pub key: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageAnalytics {
    pub totals: UsageTotals,
    pub by_day: Vec<UsageRollup>,
    pub by_month: Vec<UsageRollup>,
    pub by_profile: Vec<UsageRollup>,
    pub by_provider: Vec<UsageRollup>,
    pub by_model: Vec<UsageRollup>,
    pub by_channel: Vec<UsageRollup>,
}

impl UsageAnalytics {
    pub fn from_events(events: &[UsageEvent]) -> Self {
        let mut analytics = Self::default();
        let mut by_day = BTreeMap::<String, UsageTotals>::new();
        let mut by_month = BTreeMap::<String, UsageTotals>::new();
        let mut by_profile = BTreeMap::<String, UsageTotals>::new();
        let mut by_provider = BTreeMap::<String, UsageTotals>::new();
        let mut by_model = BTreeMap::<String, UsageTotals>::new();
        let mut by_channel = BTreeMap::<String, UsageTotals>::new();

        for event in events {
            analytics.totals.add_event(event);
            add_rollup(
                &mut by_day,
                event.timestamp.format("%Y-%m-%d").to_string(),
                event,
            );
            add_rollup(
                &mut by_month,
                event.timestamp.format("%Y-%m").to_string(),
                event,
            );
            add_rollup(&mut by_profile, event.profile_id.clone(), event);
            add_rollup(
                &mut by_provider,
                event
                    .provider
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                event,
            );
            add_rollup(
                &mut by_model,
                event.model.clone().unwrap_or_else(|| "unknown".to_string()),
                event,
            );
            add_rollup(&mut by_channel, event.channel.clone(), event);
        }

        analytics.by_day = map_rollups(by_day);
        analytics.by_month = map_rollups(by_month);
        analytics.by_profile = map_rollups(by_profile);
        analytics.by_provider = map_rollups(by_provider);
        analytics.by_model = map_rollups(by_model);
        analytics.by_channel = map_rollups(by_channel);
        analytics
    }

    pub fn merge(&mut self, other: UsageAnalytics) {
        self.totals.merge(&other.totals);
        merge_rollups(&mut self.by_day, other.by_day);
        merge_rollups(&mut self.by_month, other.by_month);
        merge_rollups(&mut self.by_profile, other.by_profile);
        merge_rollups(&mut self.by_provider, other.by_provider);
        merge_rollups(&mut self.by_model, other.by_model);
        merge_rollups(&mut self.by_channel, other.by_channel);
    }
}

fn add_rollup(map: &mut BTreeMap<String, UsageTotals>, key: String, event: &UsageEvent) {
    map.entry(key).or_default().add_event(event);
}

fn map_rollups(map: BTreeMap<String, UsageTotals>) -> Vec<UsageRollup> {
    map.into_iter()
        .map(|(key, totals)| UsageRollup { key, totals })
        .collect()
}

fn merge_rollups(target: &mut Vec<UsageRollup>, incoming: Vec<UsageRollup>) {
    let mut merged = BTreeMap::<String, UsageTotals>::new();
    for rollup in target.drain(..).chain(incoming) {
        merged.entry(rollup.key).or_default().merge(&rollup.totals);
    }
    *target = map_rollups(merged);
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub to: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBackfillReport {
    pub imported: u64,
    pub skipped_duplicates: u64,
}

#[derive(Clone)]
pub struct PersistentUsageLedger {
    path: PathBuf,
}

impl PersistentUsageLedger {
    pub fn open_sync(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir).wrap_err("failed to create usage ledger directory")?;
        let db_path = data_dir.join(USAGE_LEDGER_FILE);
        debug!(path = %db_path.display(), "prepared usage ledger");
        Ok(Self { path: db_path })
    }

    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::open_sync(data_dir)).await?
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn record(&self, event: UsageEvent) -> Result<()> {
        let db_path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let db = Self::open_database(&db_path)?;
            let body = serde_json::to_string(&event).wrap_err("failed to serialize usage event")?;
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(USAGE_EVENTS_TABLE)?;
                table.insert(event.event_id.as_str(), body.as_str())?;
            }
            Self::append_index(
                &write_txn,
                USAGE_PROFILE_INDEX_TABLE,
                &event.profile_id,
                &event.event_id,
            )?;
            Self::append_index(
                &write_txn,
                USAGE_SESSION_INDEX_TABLE,
                &event.session_id,
                &event.event_id,
            )?;
            write_txn.commit()?;
            Ok::<_, eyre::Report>(())
        })
        .await??;
        Ok(())
    }

    pub async fn backfill_events(&self, events: Vec<UsageEvent>) -> Result<UsageBackfillReport> {
        let db_path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let db = Self::open_database(&db_path)?;
            let write_txn = db.begin_write()?;
            let mut imported_events = Vec::new();
            let mut skipped_duplicates = 0_u64;
            {
                let mut table = write_txn.open_table(USAGE_EVENTS_TABLE)?;
                for event in events {
                    if table.get(event.event_id.as_str())?.is_some() {
                        skipped_duplicates = skipped_duplicates.saturating_add(1);
                        continue;
                    }
                    let body = serde_json::to_string(&event)
                        .wrap_err("failed to serialize usage event")?;
                    table.insert(event.event_id.as_str(), body.as_str())?;
                    imported_events.push(event);
                }
            }
            for event in &imported_events {
                Self::append_index(
                    &write_txn,
                    USAGE_PROFILE_INDEX_TABLE,
                    &event.profile_id,
                    &event.event_id,
                )?;
                Self::append_index(
                    &write_txn,
                    USAGE_SESSION_INDEX_TABLE,
                    &event.session_id,
                    &event.event_id,
                )?;
            }
            write_txn.commit()?;
            Ok::<_, eyre::Report>(UsageBackfillReport {
                imported: imported_events.len() as u64,
                skipped_duplicates,
            })
        })
        .await?
    }

    pub async fn list_all(&self) -> Result<Vec<UsageEvent>> {
        let db_path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let db = Self::open_database(&db_path)?;
            let read_txn = db.begin_read()?;
            Self::load_all(&read_txn)
        })
        .await?
    }

    pub async fn list_for_profile(&self, profile_id: &str) -> Result<Vec<UsageEvent>> {
        self.list_by_index(USAGE_PROFILE_INDEX_TABLE, profile_id)
            .await
    }

    pub async fn list_for_session(&self, session_id: &str) -> Result<Vec<UsageEvent>> {
        self.list_by_index(USAGE_SESSION_INDEX_TABLE, session_id)
            .await
    }

    pub async fn analytics(&self, query: UsageQuery) -> Result<UsageAnalytics> {
        let mut events = if let Some(session_id) = query.session_id.as_deref() {
            self.list_for_session(session_id).await?
        } else if let Some(profile_id) = query.profile_id.as_deref() {
            self.list_for_profile(profile_id).await?
        } else {
            self.list_all().await?
        };
        events.retain(|event| event_matches_query(event, &query));
        events.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.event_id.cmp(&b.event_id))
        });
        Ok(UsageAnalytics::from_events(&events))
    }

    pub async fn session_totals(&self, session_id: &str) -> Result<UsageTotals> {
        let events = self.list_for_session(session_id).await?;
        Ok(UsageAnalytics::from_events(&events).totals)
    }

    fn open_database(db_path: &Path) -> Result<Database> {
        let db = Database::create(db_path).wrap_err("failed to open usage ledger database")?;
        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(USAGE_EVENTS_TABLE)?;
            let _ = write_txn.open_table(USAGE_PROFILE_INDEX_TABLE)?;
            let _ = write_txn.open_table(USAGE_SESSION_INDEX_TABLE)?;
        }
        write_txn.commit()?;
        Ok(db)
    }

    fn append_index(
        txn: &redb::WriteTransaction,
        table: TableDefinition<'static, &'static str, &'static str>,
        key: &str,
        event_id: &str,
    ) -> Result<()> {
        let mut table = txn.open_table(table)?;
        let mut ids: Vec<String> = table
            .get(key)?
            .map(|value| serde_json::from_str(value.value()).unwrap_or_default())
            .unwrap_or_default();
        if !ids.iter().any(|id| id == event_id) {
            ids.push(event_id.to_string());
        }
        let ids_json =
            serde_json::to_string(&ids).wrap_err("failed to serialize usage index entry")?;
        table.insert(key, ids_json.as_str())?;
        Ok(())
    }

    fn load_by_ids(txn: &redb::ReadTransaction, ids: &[String]) -> Result<Vec<UsageEvent>> {
        let table = txn.open_table(USAGE_EVENTS_TABLE)?;
        let mut events = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(json) = table.get(id.as_str())? {
                match serde_json::from_str::<UsageEvent>(json.value()) {
                    Ok(event) => events.push(event),
                    Err(error) => {
                        warn!(event_id = id.as_str(), error = %error, "skipping corrupt usage row");
                    }
                }
            }
        }
        Ok(events)
    }

    fn load_all(txn: &redb::ReadTransaction) -> Result<Vec<UsageEvent>> {
        let table = txn.open_table(USAGE_EVENTS_TABLE)?;
        let mut events = Vec::new();
        for entry in table.iter()? {
            let (id, json) = entry?;
            match serde_json::from_str::<UsageEvent>(json.value()) {
                Ok(event) => events.push(event),
                Err(error) => {
                    warn!(event_id = id.value(), error = %error, "skipping corrupt usage row");
                }
            }
        }
        Ok(events)
    }

    async fn list_by_index(
        &self,
        table: TableDefinition<'static, &'static str, &'static str>,
        key: &str,
    ) -> Result<Vec<UsageEvent>> {
        let db_path = self.path.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let db = Self::open_database(&db_path)?;
            let read_txn = db.begin_read()?;
            let index = read_txn.open_table(table)?;
            let ids: Vec<String> = index
                .get(key.as_str())?
                .map(|value| serde_json::from_str(value.value()).unwrap_or_default())
                .unwrap_or_default();
            drop(index);
            Self::load_by_ids(&read_txn, &ids)
        })
        .await?
    }
}

fn event_matches_query(event: &UsageEvent, query: &UsageQuery) -> bool {
    if let Some(profile_id) = query.profile_id.as_deref()
        && event.profile_id != profile_id
    {
        return false;
    }
    if let Some(session_id) = query.session_id.as_deref()
        && event.session_id != session_id
    {
        return false;
    }
    if let Some(from) = query.from
        && event.timestamp < from
    {
        return false;
    }
    if let Some(to) = query.to
        && event.timestamp > to
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn event(
        profile_id: &str,
        session_id: &str,
        run_id: &str,
        provider: &str,
        model: &str,
        day: &str,
        input_tokens: u64,
        output_tokens: u64,
        cost: f64,
        channel: &str,
    ) -> UsageEvent {
        let mut event = UsageEvent::completed_run(
            profile_id,
            session_id,
            run_id,
            Some(provider.to_string()),
            Some(model.to_string()),
            Some("https://example.invalid/v1".to_string()),
            input_tokens,
            output_tokens,
            Some(cost),
            UsageCostSource::CatalogEstimate,
            channel,
            None,
        );
        event.timestamp = DateTime::parse_from_rfc3339(&format!("{day}T12:00:00Z"))
            .unwrap()
            .with_timezone(&Utc);
        event
    }

    #[tokio::test]
    async fn usage_events_survive_reopen_and_roll_up_session_totals() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = PersistentUsageLedger::open(dir.path()).await.unwrap();
        ledger
            .record(event(
                "profile-a",
                "session-a",
                "run-1",
                "openai",
                "gpt-4.1",
                "2026-05-30",
                100,
                40,
                0.012,
                "appui",
            ))
            .await
            .unwrap();
        ledger
            .record(event(
                "profile-a",
                "session-a",
                "run-2",
                "openai",
                "gpt-4.1",
                "2026-05-30",
                10,
                5,
                0.001,
                "appui",
            ))
            .await
            .unwrap();
        drop(ledger);

        let reopened = PersistentUsageLedger::open(dir.path()).await.unwrap();
        let totals = reopened.session_totals("session-a").await.unwrap();
        assert_eq!(totals.run_count, 2);
        assert_eq!(totals.input_tokens, 110);
        assert_eq!(totals.output_tokens, 45);
        assert!((totals.estimated_cost_usd - 0.013).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn analytics_group_by_time_profile_provider_model_and_channel() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = PersistentUsageLedger::open(dir.path()).await.unwrap();
        for event in [
            event(
                "profile-a",
                "session-a",
                "run-1",
                "openai",
                "gpt-4.1",
                "2026-05-01",
                100,
                50,
                0.01,
                "appui",
            ),
            event(
                "profile-a",
                "session-b",
                "run-2",
                "anthropic",
                "claude-sonnet-4",
                "2026-05-31",
                40,
                10,
                0.02,
                "matrix",
            ),
            event(
                "profile-b",
                "session-c",
                "run-3",
                "openai",
                "gpt-4.1",
                "2026-06-01",
                25,
                5,
                0.03,
                "appui",
            ),
        ] {
            ledger.record(event).await.unwrap();
        }

        let analytics = ledger.analytics(UsageQuery::default()).await.unwrap();
        assert_eq!(analytics.totals.run_count, 3);
        assert_eq!(rollup(&analytics.by_month, "2026-05").run_count, 2);
        assert_eq!(rollup(&analytics.by_day, "2026-06-01").input_tokens, 25);
        assert_eq!(rollup(&analytics.by_profile, "profile-a").run_count, 2);
        assert_eq!(rollup(&analytics.by_provider, "openai").run_count, 2);
        assert_eq!(rollup(&analytics.by_model, "gpt-4.1").output_tokens, 55);
        assert_eq!(rollup(&analytics.by_channel, "appui").run_count, 2);
    }

    #[tokio::test]
    async fn backfill_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = PersistentUsageLedger::open(dir.path()).await.unwrap();
        let mut legacy = event(
            "profile-a",
            "session-a",
            "legacy-run",
            "openai",
            "gpt-4.1",
            "2026-05-30",
            12,
            7,
            0.004,
            "backfill",
        );
        legacy.event_id = "legacy-event".to_string();

        let first = ledger.backfill_events(vec![legacy.clone()]).await.unwrap();
        let second = ledger.backfill_events(vec![legacy]).await.unwrap();

        assert_eq!(first.imported, 1);
        assert_eq!(first.skipped_duplicates, 0);
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped_duplicates, 1);
        assert_eq!(
            ledger.session_totals("session-a").await.unwrap().run_count,
            1
        );
    }

    #[tokio::test]
    async fn legacy_rows_without_schema_version_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(USAGE_LEDGER_FILE);
        let db = Database::create(&db_path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut events = tx.open_table(USAGE_EVENTS_TABLE).unwrap();
            events
                .insert(
                    "legacy",
                    r#"{"event_id":"legacy","timestamp":"2026-05-30T12:00:00Z","profile_id":"profile-a","session_id":"session-a","run_id":"run-1","provider":"openai","model":"gpt-4.1","input_tokens":3,"output_tokens":4,"estimated_cost_usd":0.01,"cost_source":"catalog_estimate","channel":"appui"}"#,
                )
                .unwrap();
            let mut sessions = tx.open_table(USAGE_SESSION_INDEX_TABLE).unwrap();
            sessions.insert("session-a", r#"["legacy"]"#).unwrap();
            let mut profiles = tx.open_table(USAGE_PROFILE_INDEX_TABLE).unwrap();
            profiles.insert("profile-a", r#"["legacy"]"#).unwrap();
        }
        tx.commit().unwrap();
        drop(db);

        let ledger = PersistentUsageLedger::open(dir.path()).await.unwrap();
        let events = ledger.list_for_session("session-a").await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].schema_version, USAGE_EVENT_SCHEMA_VERSION);
        assert_eq!(events[0].input_tokens, 3);
    }

    fn rollup<'a>(rollups: &'a [UsageRollup], key: &str) -> &'a UsageTotals {
        &rollups
            .iter()
            .find(|rollup| rollup.key == key)
            .unwrap()
            .totals
    }
}
