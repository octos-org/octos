//! Explicit adapter metadata, never prose/path guessing, opts an already
//! submitted job into the ordinary permission-checked background watcher.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Component, Path};

const MAX_ENVELOPE: usize = 256 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobReceipt {
    schema_version: u32,
    receipt_path: String,
    job_id: String,
    request_sha256: String,
    status: String,
}

impl JobReceipt {
    pub fn metadata(&self) -> Value {
        json!({"octos_external_job": self})
    }
    pub fn arguments(&self) -> Value {
        json!({"receipt_path":self.receipt_path,"job_id":self.job_id,
               "request_sha256":self.request_sha256,"timeout_secs":1800})
    }
    pub fn key(&self) -> String {
        // Tuple encoding prevents delimiter collisions.
        json!([self.receipt_path, self.job_id, self.request_sha256]).to_string()
    }
}

pub(crate) fn receipt(
    tool_name: &str,
    output: &str,
    metadata: Option<&Value>,
) -> Option<JobReceipt> {
    let raw = if let Some(value) = metadata.and_then(|m| m.get("octos_external_job")) {
        value.clone()
    } else if matches!(tool_name, "shell" | "bash" | "exec_command") && output.len() <= MAX_ENVELOPE
    {
        // CLI adapters print a complete JSON object, or a single explicit
        // event line alongside their normal output. Reading an arbitrary
        // file or mentioning a job in prose never registers observation.
        let whole = serde_json::from_str::<Value>(output).ok();
        whole
            .as_ref()
            .and_then(|v| v.get("octos_external_job"))
            .cloned()
            .or_else(|| {
                output.lines().rev().take(32).find_map(|line| {
                    let value = serde_json::from_str::<Value>(line).ok()?;
                    value.get("octos_external_job").cloned()
                })
            })?
    } else {
        return None;
    };
    if raw.to_string().len() > 8192 {
        return None;
    }
    let job: JobReceipt = serde_json::from_value(raw).ok()?;
    if job.schema_version != 1
        || !matches!(job.status.as_str(), "queued" | "pending" | "running")
        || job.receipt_path.is_empty()
        || job.receipt_path.len() > 4096
        || !Path::new(&job.receipt_path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        || job.job_id.trim().is_empty()
        || job.job_id.len() > 256
        || job.request_sha256.is_empty()
        || job.request_sha256.len() > 128
    {
        return None;
    }
    Some(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn should_require_explicit_confined_receipt_and_ignore_read_file_text() {
        let value = json!({"octos_external_job":{"schema_version":1,"receipt_path":"jobs/x.json",
            "job_id":"x","request_sha256":"hash","status":"running"}});
        assert!(receipt("shell", &value.to_string(), None).is_some());
        assert!(receipt("read_file", &value.to_string(), None).is_none());
        assert!(receipt("plugin", "", Some(&value)).is_some());
        for path in ["../x.json", "/tmp/x.json", "jobs/../../x.json"] {
            let mut invalid = value.clone();
            invalid["octos_external_job"]["receipt_path"] = path.into();
            assert!(receipt("shell", &invalid.to_string(), None).is_none());
        }
        assert!(receipt("shell", "job completed at /tmp/x.json", None).is_none());
    }
}
