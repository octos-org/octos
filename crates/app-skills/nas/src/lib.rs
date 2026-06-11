//! Synology FileStation bridge — pure helpers for request building and
//! response parsing, kept separate from I/O so they can be unit-tested
//! offline (no live NAS required).
//!
//! Entry-point conventions (DSM 6.0+):
//! - Auth uses `auth.cgi`, API `SYNO.API.Auth` version 3 (login) / 1 (logout).
//! - FileStation List/Download/Search use the unified `entry.cgi`, version 2.
//! - Session is carried as the `_sid` query parameter (login `format=sid`).

use std::collections::BTreeMap;

use serde::Deserialize;

/// Maximum size (bytes) a file may be for `nas_read_file` to return its text.
pub const MAX_READ_BYTES: u64 = 1024 * 1024; // 1 MiB

// ---------------------------------------------------------------------------
// URL / query building
// ---------------------------------------------------------------------------

/// Percent-encode a single string value for use in a query string.
/// Encodes everything that is not an RFC 3986 unreserved character.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Encode a list of strings as a JSON array literal, e.g. `["/video"]`.
/// This is the form FileStation expects for `path` / `folder_path` /
/// `additional` parameters (the raw, un-encoded value).
pub fn json_array(items: &[&str]) -> String {
    serde_json::to_string(items).expect("string vec always serializes")
}

/// Encode a single string as a JSON scalar literal, e.g. `"open"`.
pub fn json_scalar(value: &str) -> String {
    serde_json::to_string(value).expect("string always serializes")
}

/// Build a `webapi` URL. `cgi` is the cgi file (e.g. `auth.cgi`,
/// `entry.cgi`). `params` are appended in sorted order and URL-encoded.
/// Values are expected to be the *raw* (un-encoded) values; encoding happens
/// here. Sorted order makes the output deterministic for tests.
pub fn build_url(base: &str, cgi: &str, params: &BTreeMap<&str, String>) -> String {
    let base = base.trim_end_matches('/');
    let mut url = format!("{base}/webapi/{cgi}");
    let mut first = true;
    for (k, v) in params {
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str(k);
        url.push('=');
        url.push_str(&urlencode(v));
    }
    url
}

/// Build the login URL for `SYNO.API.Auth` (version 3, `format=sid`).
pub fn login_url(base: &str, account: &str, passwd: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.API.Auth".to_string());
    p.insert("version", "3".to_string());
    p.insert("method", "login".to_string());
    p.insert("account", account.to_string());
    p.insert("passwd", passwd.to_string());
    p.insert("session", "FileStation".to_string());
    p.insert("format", "sid".to_string());
    build_url(base, "auth.cgi", &p)
}

/// Build the logout URL for `SYNO.API.Auth`.
pub fn logout_url(base: &str, sid: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.API.Auth".to_string());
    p.insert("version", "1".to_string());
    p.insert("method", "logout".to_string());
    p.insert("session", "FileStation".to_string());
    p.insert("_sid", sid.to_string());
    build_url(base, "auth.cgi", &p)
}

/// Build the `list_share` URL (lists shared folders).
pub fn list_share_url(base: &str, sid: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.FileStation.List".to_string());
    p.insert("version", "2".to_string());
    p.insert("method", "list_share".to_string());
    p.insert("_sid", sid.to_string());
    build_url(base, "entry.cgi", &p)
}

/// Build the `list` URL for a folder's contents.
pub fn list_folder_url(base: &str, sid: &str, folder_path: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.FileStation.List".to_string());
    p.insert("version", "2".to_string());
    p.insert("method", "list".to_string());
    p.insert("folder_path", json_array(&[folder_path]));
    p.insert("additional", json_array(&["size", "time", "type"]));
    p.insert("_sid", sid.to_string());
    build_url(base, "entry.cgi", &p)
}

/// Build the `download` URL for a single path.
pub fn download_url(base: &str, sid: &str, path: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.FileStation.Download".to_string());
    p.insert("version", "2".to_string());
    p.insert("method", "download".to_string());
    p.insert("path", json_array(&[path]));
    p.insert("mode", json_scalar("download"));
    p.insert("_sid", sid.to_string());
    build_url(base, "entry.cgi", &p)
}

/// Build the search `start` URL (returns a task id).
pub fn search_start_url(base: &str, sid: &str, folder: &str, pattern: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.FileStation.Search".to_string());
    p.insert("version", "2".to_string());
    p.insert("method", "start".to_string());
    p.insert("folder_path", json_array(&[folder]));
    p.insert("pattern", pattern.to_string());
    p.insert("recursive", "true".to_string());
    p.insert("_sid", sid.to_string());
    build_url(base, "entry.cgi", &p)
}

/// Build the search `list` URL (poll for results).
pub fn search_list_url(base: &str, sid: &str, taskid: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.FileStation.Search".to_string());
    p.insert("version", "2".to_string());
    p.insert("method", "list".to_string());
    p.insert("taskid", json_scalar(taskid));
    p.insert("limit", "-1".to_string());
    p.insert("additional", json_array(&["size", "time"]));
    p.insert("_sid", sid.to_string());
    build_url(base, "entry.cgi", &p)
}

/// Build a search cleanup URL (`stop` or `clean`).
pub fn search_cleanup_url(base: &str, sid: &str, method: &str, taskid: &str) -> String {
    let mut p = BTreeMap::new();
    p.insert("api", "SYNO.FileStation.Search".to_string());
    p.insert("version", "2".to_string());
    p.insert("method", method.to_string());
    p.insert("taskid", json_scalar(taskid));
    p.insert("_sid", sid.to_string());
    build_url(base, "entry.cgi", &p)
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    success: bool,
    #[serde(default = "none")]
    data: Option<T>,
    #[serde(default)]
    error: Option<ApiError>,
}

fn none<T>() -> Option<T> {
    None
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    code: i64,
}

#[derive(Debug, Deserialize)]
struct LoginData {
    sid: String,
}

#[derive(Debug, Deserialize)]
struct ShareListData {
    #[serde(default)]
    shares: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct FileListData {
    #[serde(default)]
    files: Vec<Entry>,
    #[serde(default)]
    finished: Option<bool>,
}

/// A single file/folder/share entry as returned by List & Search.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Entry {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub isdir: bool,
    #[serde(default)]
    pub additional: Option<Additional>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Additional {
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub time: Option<TimeInfo>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TimeInfo {
    #[serde(default)]
    pub mtime: Option<i64>,
}

/// Map a Synology error code to a friendly message.
pub fn error_message(code: i64) -> String {
    let detail = match code {
        100 => "unknown error",
        101 => "invalid parameter / no such API, method or version",
        102 => "the requested API does not exist",
        103 => "the requested method does not exist",
        104 => "the requested version is not supported",
        105 => "session does not have permission",
        106 => "session timeout",
        107 => "session interrupted by duplicate login",
        119 => "session ID (SID) not found",
        400 => "no such account or incorrect password",
        401 => "account disabled",
        402 => "permission denied",
        403 => "2-step verification required (WebAPI cannot use 2FA accounts)",
        404 => "2-step verification failed",
        407 => "operation not permitted",
        408 => "no such file or directory",
        414 => "file already exists",
        415 => "disk quota exceeded",
        416 => "no space left on device",
        418 => "illegal name or path",
        _ => "operation failed",
    };
    let hint = match code {
        105 | 106 | 119 => " (auth/session problem — check NAS_USER/NAS_PASS; 2FA must be off)",
        408 => " (path not found)",
        _ => "",
    };
    format!("Synology error {code}: {detail}{hint}")
}

fn envelope<T>(body: &str) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let env: ApiEnvelope<T> =
        serde_json::from_str(body).map_err(|e| format!("failed to parse NAS response: {e}"))?;
    if env.success {
        env.data
            .ok_or_else(|| "NAS response missing 'data'".to_string())
    } else {
        let code = env.error.map(|e| e.code).unwrap_or(0);
        Err(error_message(code))
    }
}

/// Parse a login response, returning the session id.
pub fn parse_login(body: &str) -> Result<String, String> {
    let data: LoginData = envelope(body)?;
    Ok(data.sid)
}

/// Parse a `list_share` response into its share entries.
pub fn parse_share_list(body: &str) -> Result<Vec<Entry>, String> {
    let data: ShareListData = envelope(body)?;
    Ok(data.shares)
}

/// Parse a `list` (folder contents) response into file entries.
pub fn parse_file_list(body: &str) -> Result<Vec<Entry>, String> {
    let data: FileListData = envelope(body)?;
    Ok(data.files)
}

/// Parse a search `start` response, returning the task id.
pub fn parse_search_start(body: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct TaskData {
        taskid: String,
    }
    let data: TaskData = envelope(body)?;
    Ok(data.taskid)
}

/// Parse a search `list` response into (files, finished).
pub fn parse_search_list(body: &str) -> Result<(Vec<Entry>, bool), String> {
    let data: FileListData = envelope(body)?;
    Ok((data.files, data.finished.unwrap_or(true)))
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Human-readable byte size.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Render a list of entries as a readable text listing.
pub fn format_listing(entries: &[Entry], header: &str) -> String {
    if entries.is_empty() {
        return format!("{header}\n(empty)");
    }
    let mut lines = vec![header.to_string()];
    for e in entries {
        if e.isdir {
            lines.push(format!("[DIR]  {}", e.name));
        } else {
            let size = e
                .additional
                .as_ref()
                .and_then(|a| a.size)
                .map(human_size)
                .unwrap_or_else(|| "?".to_string());
            lines.push(format!("       {}  ({size})", e.name));
        }
    }
    lines.join("\n")
}

/// Decide whether downloaded bytes are returnable text, or produce an error.
pub fn text_from_bytes(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(format!(
            "file too large to read as text ({} bytes, max {} bytes)",
            bytes.len(),
            MAX_READ_BYTES
        ));
    }
    if bytes.contains(&0) {
        return Err("binary file: contains NUL bytes, not readable as text".to_string());
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Err("binary file: not valid UTF-8 text".to_string()),
    }
}

/// Detect a JSON error body returned in place of file bytes by Download.
pub fn download_body_is_error(content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    let looks_json = content_type
        .map(|c| c.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false);
    if looks_json || bytes.starts_with(b"{") {
        if let Ok(text) = std::str::from_utf8(bytes) {
            #[derive(Deserialize)]
            struct Err2 {
                success: bool,
                #[serde(default)]
                error: Option<ApiError>,
            }
            if let Ok(parsed) = serde_json::from_str::<Err2>(text) {
                if !parsed.success {
                    let code = parsed.error.map(|e| e.code).unwrap_or(0);
                    return Some(error_message(code));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_encode_json_array_for_paths() {
        assert_eq!(json_array(&["/video"]), r#"["/video"]"#);
        assert_eq!(
            json_array(&["size", "time", "type"]),
            r#"["size","time","type"]"#
        );
    }

    #[test]
    fn should_build_login_url_with_format_sid() {
        let url = login_url("https://nas.example.com:5001", "admin", "p@ss/w,d");
        assert!(url.starts_with("https://nas.example.com:5001/webapi/auth.cgi?"));
        assert!(url.contains("api=SYNO.API.Auth"));
        assert!(url.contains("version=3"));
        assert!(url.contains("method=login"));
        assert!(url.contains("account=admin"));
        assert!(url.contains("session=FileStation"));
        assert!(url.contains("format=sid"));
        // password is URL-encoded (slash, comma, @ all escaped)
        assert!(url.contains("passwd=p%40ss%2Fw%2Cd"));
    }

    #[test]
    fn should_strip_trailing_slash_from_base() {
        let url = list_share_url("https://nas:5001/", "SID123");
        assert!(url.starts_with("https://nas:5001/webapi/entry.cgi?"));
        assert!(!url.contains("//webapi"));
    }

    #[test]
    fn should_build_list_share_url() {
        let url = list_share_url("https://nas:5001", "SID123");
        assert!(url.contains("/webapi/entry.cgi?"));
        assert!(url.contains("api=SYNO.FileStation.List"));
        assert!(url.contains("version=2"));
        assert!(url.contains("method=list_share"));
        assert!(url.contains("_sid=SID123"));
    }

    #[test]
    fn should_build_list_folder_url_with_encoded_json_arrays() {
        let url = list_folder_url("https://nas:5001", "SID", "/video");
        // folder_path=["/video"] -> %5B%22%2Fvideo%22%5D
        assert!(url.contains("folder_path=%5B%22%2Fvideo%22%5D"));
        // additional=["size","time","type"]
        assert!(url.contains("additional=%5B%22size%22%2C%22time%22%2C%22type%22%5D"));
        assert!(url.contains("method=list"));
    }

    #[test]
    fn should_build_download_url_in_download_mode() {
        let url = download_url("https://nas:5001", "SID", "/photo/a.txt");
        assert!(url.contains("api=SYNO.FileStation.Download"));
        assert!(url.contains("method=download"));
        // path=["/photo/a.txt"]
        assert!(url.contains("path=%5B%22%2Fphoto%2Fa.txt%22%5D"));
        // mode="download"
        assert!(url.contains("mode=%22download%22"));
    }

    #[test]
    fn should_build_search_start_and_list_urls() {
        let s = search_start_url("https://nas:5001", "SID", "/video", "report");
        assert!(s.contains("method=start"));
        assert!(s.contains("folder_path=%5B%22%2Fvideo%22%5D"));
        assert!(s.contains("pattern=report"));
        assert!(s.contains("recursive=true"));

        let l = search_list_url("https://nas:5001", "SID", "TASK1");
        assert!(l.contains("method=list"));
        assert!(l.contains("taskid=%22TASK1%22"));
        assert!(l.contains("limit=-1"));
    }

    #[test]
    fn should_parse_login_sid() {
        let body = r#"{"success":true,"data":{"sid":"abc123"}}"#;
        assert_eq!(parse_login(body).unwrap(), "abc123");
    }

    #[test]
    fn should_map_error_code_on_failure() {
        let body = r#"{"success":false,"error":{"code":119}}"#;
        let err = parse_login(body).unwrap_err();
        assert!(err.contains("119"));
        assert!(err.contains("SID"));
    }

    #[test]
    fn should_parse_share_list() {
        let body = r#"{"success":true,"data":{"total":2,"offset":0,"shares":[
            {"isdir":true,"name":"video","path":"/video"},
            {"isdir":true,"name":"photo","path":"/photo"}]}}"#;
        let shares = parse_share_list(body).unwrap();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].name, "video");
        assert!(shares[0].isdir);
    }

    #[test]
    fn should_parse_file_list_with_additional() {
        let body = r#"{"success":true,"data":{"total":1,"offset":0,"files":[
            {"path":"/video/2.txt","name":"2.txt","isdir":false,
             "additional":{"size":12800,"time":{"mtime":1369964408},"type":"TXT"}}]}}"#;
        let files = parse_file_list(body).unwrap();
        assert_eq!(files.len(), 1);
        let add = files[0].additional.as_ref().unwrap();
        assert_eq!(add.size, Some(12800));
        assert_eq!(add.kind.as_deref(), Some("TXT"));
        assert_eq!(add.time.as_ref().unwrap().mtime, Some(1369964408));
    }

    #[test]
    fn should_parse_search_start_taskid() {
        let body = r#"{"success":true,"data":{"taskid":"51CE617CF57B24E5"}}"#;
        assert_eq!(parse_search_start(body).unwrap(), "51CE617CF57B24E5");
    }

    #[test]
    fn should_parse_search_list_finished_flag() {
        let body = r#"{"success":true,"data":{"total":0,"offset":0,"finished":false,"files":[]}}"#;
        let (files, finished) = parse_search_list(body).unwrap();
        assert!(files.is_empty());
        assert!(!finished);
    }

    #[test]
    fn should_reject_oversize_text() {
        let big = vec![b'a'; (MAX_READ_BYTES + 1) as usize];
        let err = text_from_bytes(&big).unwrap_err();
        assert!(err.contains("too large"));
    }

    #[test]
    fn should_reject_binary_with_nul() {
        let err = text_from_bytes(&[0x41, 0x00, 0x42]).unwrap_err();
        assert!(err.contains("binary"));
    }

    #[test]
    fn should_reject_invalid_utf8() {
        let err = text_from_bytes(&[0xff, 0xfe, 0xfd]).unwrap_err();
        assert!(err.contains("binary"));
    }

    #[test]
    fn should_accept_valid_utf8_text() {
        assert_eq!(text_from_bytes(b"hello world").unwrap(), "hello world");
    }

    #[test]
    fn should_detect_json_error_body_from_download() {
        let body = br#"{"success":false,"error":{"code":408}}"#;
        let msg = download_body_is_error(Some("application/json"), body).unwrap();
        assert!(msg.contains("408"));
    }

    #[test]
    fn should_not_flag_plain_text_as_download_error() {
        assert!(download_body_is_error(Some("text/plain"), b"hello").is_none());
    }

    #[test]
    fn should_format_human_size() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
    }

    #[test]
    fn should_format_listing_with_dirs_and_files() {
        let entries = vec![
            Entry {
                path: "/d".into(),
                name: "docs".into(),
                isdir: true,
                additional: None,
            },
            Entry {
                path: "/d/a.txt".into(),
                name: "a.txt".into(),
                isdir: false,
                additional: Some(Additional {
                    size: Some(2048),
                    time: None,
                    kind: Some("TXT".into()),
                }),
            },
        ];
        let out = format_listing(&entries, "Listing of /");
        assert!(out.contains("[DIR]  docs"));
        assert!(out.contains("a.txt"));
        assert!(out.contains("2.0 KB"));
    }
}
