//! Space (class/course) data model types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A class or course space that groups members and notebooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Space {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub owner_id: String,
    #[serde(default)]
    pub member_ids: Vec<String>,
    #[serde(default)]
    pub notebook_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
}
