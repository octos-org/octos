use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use eyre::{Result, WrapErr};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ADMIN_AUDIT_SCHEMA_VERSION: u32 = 1;

const ADMIN_AUDIT_TABLE: TableDefinition<&str, &str> = TableDefinition::new("admin_audit_v1");
const DEFAULT_AUDIT_LIMIT: usize = 50;
const MAX_AUDIT_LIMIT: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdminAuditEntry {
    pub schema_version: u32,
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_summary: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_summary: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct AdminAuditRecordInput {
    pub actor: String,
    pub action: String,
    pub target_id: String,
    pub before_summary: Option<Value>,
    pub after_summary: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct AdminAuditQuery {
    pub actor: Option<String>,
    pub action: Option<String>,
    pub target_id: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for AdminAuditQuery {
    fn default() -> Self {
        Self {
            actor: None,
            action: None,
            target_id: None,
            from: None,
            to: None,
            limit: DEFAULT_AUDIT_LIMIT,
            offset: 0,
        }
    }
}

impl AdminAuditQuery {
    pub fn bounded_limit(&self) -> usize {
        self.limit.clamp(1, MAX_AUDIT_LIMIT)
    }

    fn matches(&self, entry: &AdminAuditEntry) -> bool {
        if self
            .actor
            .as_deref()
            .is_some_and(|actor| entry.actor != actor)
        {
            return false;
        }
        if self
            .action
            .as_deref()
            .is_some_and(|action| entry.action != action)
        {
            return false;
        }
        if self
            .target_id
            .as_deref()
            .is_some_and(|target| entry.target_id != target)
        {
            return false;
        }
        if self.from.is_some_and(|from| entry.timestamp < from) {
            return false;
        }
        if self.to.is_some_and(|to| entry.timestamp > to) {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdminAuditPage {
    pub entries: Vec<AdminAuditEntry>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

pub struct AdminAuditStore {
    db: Arc<Database>,
}

impl AdminAuditStore {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)
            .wrap_err_with(|| format!("failed to create data dir: {}", data_dir.display()))?;
        let db_path = data_dir.join("admin_audit.redb");
        let db = Database::create(&db_path)
            .wrap_err_with(|| format!("failed to open admin audit store: {}", db_path.display()))?;
        let write_txn = db
            .begin_write()
            .wrap_err("failed to initialize admin audit store")?;
        {
            let _table = write_txn
                .open_table(ADMIN_AUDIT_TABLE)
                .wrap_err("failed to initialize admin audit table")?;
        }
        write_txn
            .commit()
            .wrap_err("failed to commit admin audit table initialization")?;
        Ok(Self { db: Arc::new(db) })
    }

    pub fn record(&self, input: AdminAuditRecordInput) -> Result<AdminAuditEntry> {
        let timestamp = Utc::now();
        let id = uuid::Uuid::now_v7().to_string();
        let entry = AdminAuditEntry {
            schema_version: ADMIN_AUDIT_SCHEMA_VERSION,
            id,
            timestamp,
            actor: input.actor,
            action: input.action,
            target_id: input.target_id,
            before_summary: input.before_summary,
            after_summary: input.after_summary,
        };
        let key = format!("{:020}:{}", timestamp.timestamp_micros(), entry.id);
        let value = serde_json::to_string(&entry).wrap_err("failed to serialize audit entry")?;

        let write_txn = self
            .db
            .begin_write()
            .wrap_err("failed to begin admin audit write")?;
        {
            let mut table = write_txn
                .open_table(ADMIN_AUDIT_TABLE)
                .wrap_err("failed to open admin audit table")?;
            table
                .insert(key.as_str(), value.as_str())
                .wrap_err("failed to insert admin audit entry")?;
        }
        write_txn
            .commit()
            .wrap_err("failed to commit admin audit entry")?;
        Ok(entry)
    }

    pub fn list(&self, query: AdminAuditQuery) -> Result<AdminAuditPage> {
        let read_txn = self
            .db
            .begin_read()
            .wrap_err("failed to begin admin audit read")?;
        let table = read_txn
            .open_table(ADMIN_AUDIT_TABLE)
            .wrap_err("failed to open admin audit table")?;
        let mut entries = Vec::new();
        for row in table.iter().wrap_err("failed to scan admin audit table")? {
            let (_key, value) = row.wrap_err("failed to read admin audit row")?;
            match serde_json::from_str::<AdminAuditEntry>(value.value()) {
                Ok(entry) if query.matches(&entry) => entries.push(entry),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "skipping invalid admin audit entry");
                }
            }
        }

        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
        let total = entries.len();
        let limit = query.bounded_limit();
        let offset = query.offset;
        let entries = entries.into_iter().skip(offset).take(limit).collect();

        Ok(AdminAuditPage {
            entries,
            total,
            limit,
            offset,
        })
    }
}

pub fn parse_audit_datetime(value: &str, date_end: bool) -> Result<DateTime<Utc>, String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }

    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("invalid audit timestamp '{value}'"))?;
    let naive = if date_end {
        date.and_hms_milli_opt(23, 59, 59, 999)
    } else {
        date.and_hms_opt(0, 0, 0)
    }
    .ok_or_else(|| format!("invalid audit date '{value}'"))?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_lists_and_filters_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = AdminAuditStore::open(dir.path()).unwrap();

        store
            .record(AdminAuditRecordInput {
                actor: "admin-token".into(),
                action: "allowlist.add".into(),
                target_id: "ada@example.com".into(),
                before_summary: None,
                after_summary: Some(json!({ "email": "ada@example.com" })),
            })
            .unwrap();
        store
            .record(AdminAuditRecordInput {
                actor: "user:root".into(),
                action: "profile.delete".into(),
                target_id: "demo".into(),
                before_summary: Some(json!({ "id": "demo" })),
                after_summary: None,
            })
            .unwrap();

        let all = store.list(AdminAuditQuery::default()).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.entries[0].action, "profile.delete");

        let filtered = store
            .list(AdminAuditQuery {
                action: Some("allowlist.add".into()),
                ..AdminAuditQuery::default()
            })
            .unwrap();
        assert_eq!(filtered.total, 1);
        assert_eq!(filtered.entries[0].target_id, "ada@example.com");
    }

    #[test]
    fn parses_rfc3339_and_date_bounds() {
        assert!(parse_audit_datetime("2026-05-24T12:00:00Z", false).is_ok());
        let start = parse_audit_datetime("2026-05-24", false).unwrap();
        let end = parse_audit_datetime("2026-05-24", true).unwrap();
        assert!(end > start);
    }
}
