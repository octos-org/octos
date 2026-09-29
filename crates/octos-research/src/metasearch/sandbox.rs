//! Runs engine scripts in the bounded OctoScript runtime.
//!
//! Each call gets a fresh `octoscript-core` runtime with bounded
//! instructions, heap, strings, stack and wall-clock time. Besides the
//! frozen, effect-free `mod.std.*` library, the host installs exactly two
//! frozen modules:
//!
//! - `net`: `net.request({url, method, headers, body})` validates a request
//!   against the engine's declared hosts and returns it; `net.url({base,
//!   query})` builds a percent-encoded URL on a declared host. A record
//!   `query` carries no key-order contract; pass a list of `[name, value]`
//!   pairs when order matters. Neither opens a connection: the core
//!   performs the request after the script returns.
//! - `markup`: `markup.feed({lang})` parses the response being handled as
//!   RSS/Atom (the body stays in the host), `markup.text({html})` turns an
//!   HTML fragment into plain text, and `markup.matches({query, text})` says
//!   whether a headline is about the query (the phrase, or every
//!   significant term; see [`super::topic`]).
//!
//! Each method has a call budget and bounded JSON input and output. There is
//! no `mod.tool`, filesystem, process, clock or network module.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

// The native-method macros expand to these names unqualified.
#[allow(unused_imports)]
use makepad_script::{
    LiveId, NIL, ScriptIp, ScriptValue, id, id_lut, script_args_def, script_err_not_allowed,
    script_value,
};
use octoscript_core::{
    ExecutionLimits, Runtime, decode_bounded_script_json, encode_bounded_script_json, vm,
};
use serde_json::{Value, json};
use url::Url;

/// Largest response body handed to `parse_response`.
pub const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 64;

/// A request the script built, already checked against the host list.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Load the page in a real browser (results pages that need
    /// JavaScript); the host's fetcher decides how.
    pub render: bool,
}

/// What `parse_response` returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScriptParse {
    pub items: Vec<Value>,
    /// Provider asked the client to wait (e.g. Stack Exchange `backoff`).
    pub backoff: Option<Duration>,
    /// The engine recognised an error answer (e.g. a plain-text rate-limit
    /// notice with a 200 status).
    pub error: Option<String>,
    /// The page is a bot challenge (CAPTCHA, "unusual traffic"): the reason.
    /// It goes to the person to solve; it is never worked around.
    pub challenge: Option<String>,
}

/// The engine as the sandbox sees it: its source and where it may go.
pub struct SandboxEngine<'a> {
    pub id: &'a str,
    pub source: &'a str,
    pub allowed_hosts: &'a [String],
    pub allow_http: bool,
}

/// Headers the host owns; a script cannot set them.
const HOST_HEADERS: &[&str] = &[
    "user-agent",
    "authorization",
    "cookie",
    "proxy-authorization",
    "host",
    "x-subscription-token",
    "x-api-key",
];

/// Check that `raw` is an http(s) URL on one of `allowed` hosts.
pub fn check_url(raw: &str, allowed: &[String], allow_http: bool) -> Result<Url, String> {
    let u = Url::parse(raw.trim()).map_err(|e| format!("invalid URL {raw:?}: {e}"))?;
    match u.scheme() {
        "https" => {}
        "http" if allow_http => {}
        s => return Err(format!("scheme {s:?} is not allowed")),
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("credentials in URLs are not allowed".to_string());
    }
    if u.port().is_some() {
        return Err("explicit ports are not allowed".to_string());
    }
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    if !allowed.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Err(format!(
            "host {host:?} is not declared in the engine manifest"
        ));
    }
    Ok(u)
}

fn limits() -> ExecutionLimits {
    ExecutionLimits {
        max_string_bytes: MAX_BODY_BYTES + 64 * 1024,
        max_heap_bytes: 64 * 1024 * 1024,
        instruction_limit: 5_000_000,
        soft_timeout: Duration::from_millis(250),
        hard_timeout: Duration::from_millis(500),
        ..ExecutionLimits::default()
    }
}

fn request_tool(input: &Value, allowed: &[String], allow_http: bool) -> Result<Value, String> {
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .ok_or("net.request needs a url")?;
    let u = check_url(url, allowed, allow_http)?;
    let method = input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    if method != "GET" && method != "POST" {
        return Err(format!("method {method} is not allowed"));
    }
    let mut headers = serde_json::Map::new();
    if let Some(h) = input.get("headers").and_then(Value::as_object) {
        for (k, v) in h {
            let name = k.to_ascii_lowercase().replace('_', "-");
            if HOST_HEADERS.contains(&name.as_str()) {
                return Err(format!("header {name:?} is set by the host"));
            }
            let v = v.as_str().ok_or("header values must be strings")?;
            if v.contains(['\r', '\n']) {
                return Err("header values must be single-line".to_string());
            }
            headers.insert(name, Value::String(v.to_string()));
        }
    }
    let body = input.get("body").and_then(Value::as_str);
    let render = input
        .get("render")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if render && method != "GET" {
        return Err("a rendered request must be a GET".to_string());
    }
    Ok(json!({
        "method": method,
        "url": u.as_str(),
        "headers": headers,
        "body": body,
        "render": render,
    }))
}

/// `markup.select({item, fields})`: select elements of the response being
/// handled (HTML) and pull named fields out of each. `item` is a CSS
/// selector for one result; each field is `"<css>"` (text of the first
/// match inside the item), `"<css>@<attr>"` (its attribute), `"@<attr>"`
/// (the item's own attribute), `""` (the item's text), or `"^<css>..."`
/// to look in the closest enclosing element that matches instead of inside
/// the item (e.g. `"^a@href"` for a headline's link). At most 100 items.
fn select_tool(input: &Value, body: Option<&str>) -> Result<Value, String> {
    let body = body.ok_or("markup.select is only available in parse_response")?;
    let item_sel = input
        .get("item")
        .and_then(Value::as_str)
        .ok_or("markup.select needs an item selector")?;
    let item = scraper::Selector::parse(item_sel)
        .map_err(|e| format!("bad selector {item_sel:?}: {e}"))?;
    let mut fields = Vec::new();
    if let Some(m) = input.get("fields").and_then(Value::as_object) {
        for (name, spec) in m {
            let spec = spec.as_str().ok_or("field specs must be strings")?;
            let (spec, closest) = match spec.trim().strip_prefix('^') {
                Some(rest) => (rest, true),
                None => (spec.trim(), false),
            };
            let (css, attr) = match spec.rsplit_once('@') {
                Some((css, attr)) => (css.trim(), Some(attr.trim().to_string())),
                None => (spec.trim(), None),
            };
            let sel = if css.is_empty() {
                None
            } else {
                Some(
                    scraper::Selector::parse(css)
                        .map_err(|e| format!("bad selector {css:?}: {e}"))?,
                )
            };
            fields.push((name.clone(), sel, attr, closest));
        }
    }
    let doc = scraper::Html::parse_document(body);
    let clip = |t: String| -> String {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        t.chars().take(1000).collect()
    };
    let mut items = Vec::new();
    let mut used = 64;
    for el in doc.select(&item).take(100) {
        let mut rec = serde_json::Map::new();
        for (name, sel, attr, closest) in &fields {
            let target = match (sel, closest) {
                (Some(sel), true) => el
                    .ancestors()
                    .filter_map(scraper::ElementRef::wrap)
                    .find(|a| sel.matches(a)),
                (Some(sel), false) => el.select(sel).next(),
                (None, _) => Some(el),
            };
            let value = target.and_then(|t| match attr {
                Some(a) => t.value().attr(a).map(String::from),
                None => Some(t.text().collect::<String>()),
            });
            rec.insert(
                name.clone(),
                value.map(clip).map(Value::String).unwrap_or(Value::Null),
            );
        }
        let rec = Value::Object(rec);
        used += rec.to_string().len() + 1;
        if used > BRIDGE_BYTES - 1024 {
            break;
        }
        items.push(rec);
    }
    Ok(json!({ "items": items }))
}

/// `markup.unwrap({url})`: the destination of a search engine's redirect
/// link (DuckDuckGo `/l/?uddg=`, Bing `/ck/a?u=a1<base64url>`, Google
/// `/url?q=`); other URLs come back unchanged. Relative and
/// protocol-relative links are resolved against `base` when given.
fn unwrap_tool(input: &Value) -> Result<Value, String> {
    let raw = input
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let base = input.get("base").and_then(Value::as_str);
    Ok(json!({ "url": unwrap_redirect(raw, base) }))
}

pub(crate) fn unwrap_redirect(raw: &str, base: Option<&str>) -> String {
    use base64::Engine as _;
    let joined = match (Url::parse(raw), base.and_then(|b| Url::parse(b).ok())) {
        (Ok(u), _) => Some(u),
        (Err(_), Some(b)) => b.join(raw).ok(),
        (Err(_), None) if raw.starts_with("//") => Url::parse(&format!("https:{raw}")).ok(),
        _ => None,
    };
    let Some(u) = joined else {
        return raw.to_string();
    };
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    let param = |k: &str| {
        u.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.into_owned())
    };
    let target = if host.ends_with("duckduckgo.com") && u.path().starts_with("/l/") {
        param("uddg")
    } else if host.ends_with("bing.com") && u.path().starts_with("/ck/") {
        param("u").and_then(|v| {
            let b64 = v.strip_prefix("a1")?;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b64.trim_end_matches('='))
                .ok()?;
            String::from_utf8(bytes).ok()
        })
    } else if host.contains("google.") && u.path() == "/url" {
        param("q").or_else(|| param("url"))
    } else {
        None
    };
    match target {
        Some(t) if t.starts_with("http://") || t.starts_with("https://") => t,
        _ => u.to_string(),
    }
}

fn url_tool(input: &Value, allowed: &[String], allow_http: bool) -> Result<Value, String> {
    let base = input
        .get("base")
        .and_then(Value::as_str)
        .ok_or("net.url needs a base")?;
    let mut u = check_url(base, allowed, allow_http)?;
    let pairs: Vec<(String, String)> = match input.get("query") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), scalar(v)?)))
            .collect(),
        // Ordered form: [[name, value], ...].
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|p| {
                let p = p.as_array()?;
                Some((p.first()?.as_str()?.to_string(), scalar(p.get(1)?)?))
            })
            .collect(),
        Some(_) => return Err("net.url query must be a record or a list of pairs".to_string()),
    };
    if !pairs.is_empty() {
        u.query_pairs_mut().extend_pairs(pairs);
    }
    Ok(json!({ "url": u.as_str() }))
}

/// Scalar query value as text; `nil` drops the parameter.
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Bound on one module call's JSON input or output.
const BRIDGE_BYTES: usize = 256 * 1024;
const BRIDGE_DEPTH: usize = 32;

/// Parse the response body being handled as RSS/Atom. The body stays in the
/// host; the script gets entries with snippets cut to 400 characters, as
/// many as fit the bridge bound.
fn feed_tool(input: &Value, body: Option<&str>) -> Result<Value, String> {
    let body = body.ok_or("markup.feed is only available in parse_response")?;
    let lang = input.get("lang").and_then(Value::as_str);
    let mut used = 64;
    let mut entries = Vec::new();
    for mut e in crate::providers::parse_feed_entries(body, lang)? {
        if let Some(s) = e.get("snippet").and_then(Value::as_str) {
            if s.chars().count() > 400 {
                let cut: String = s.chars().take(400).collect();
                e["snippet"] = json!(cut + "…");
            }
        }
        used += e.to_string().len() + 1;
        if used > BRIDGE_BYTES - 1024 {
            break;
        }
        entries.push(e);
    }
    Ok(json!({ "entries": entries }))
}

fn text_tool(input: &Value) -> Result<Value, String> {
    let html = input.get("html").and_then(Value::as_str).unwrap_or("");
    let mut text = crate::providers::html_to_text(html);
    if text.chars().count() > 4000 {
        text = text.chars().take(4000).collect::<String>() + "…";
    }
    Ok(json!({ "text": text }))
}

/// `{matched}`: whether `text` is about `query` ([`super::topic`]).
fn matches_tool(input: &Value) -> Result<Value, String> {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .ok_or("markup.matches needs a query")?;
    let text = input.get("text").and_then(Value::as_str).unwrap_or("");
    Ok(json!({ "matched": super::topic::matches_query(query, text) }))
}

type Handler = Box<dyn Fn(&Value) -> Result<Value, String>>;

/// Install a frozen host module whose methods take and return one bounded
/// JSON record, each with a call budget.
fn install_module(rt: &mut Runtime, name: &str, methods: Vec<(&'static str, usize, Handler)>) {
    rt.configure(|vm: &mut vm::ScriptVm| {
        let module = vm.new_module(LiveId::from_str(name));
        for (method, max_calls, handler) in methods {
            let calls = Rc::new(Cell::new(0usize));
            let label = format!("{name}.{method}");
            vm.add_method(
                module,
                LiveId::from_str(method),
                script_args_def!(input = NIL),
                move |vm, args| {
                    if calls.get() >= max_calls {
                        return script_err_not_allowed!(
                            vm.bx.threads.cur_ref().trap,
                            "{} call budget ({}) used up",
                            label,
                            max_calls
                        );
                    }
                    calls.set(calls.get() + 1);
                    let input = script_value!(vm, args.input);
                    let encoded =
                        match encode_bounded_script_json(vm, input, BRIDGE_BYTES, BRIDGE_DEPTH) {
                            Ok(e) => e,
                            Err(e) => {
                                return script_err_not_allowed!(
                                    vm.bx.threads.cur_ref().trap,
                                    "{} expects a bounded JSON record: {}",
                                    label,
                                    e
                                );
                            }
                        };
                    let value: Value = match serde_json::from_str(&encoded) {
                        Ok(Value::Object(m)) => Value::Object(m),
                        _ => {
                            return script_err_not_allowed!(
                                vm.bx.threads.cur_ref().trap,
                                "{} expects a record",
                                label
                            );
                        }
                    };
                    match handler(&value) {
                        Ok(out) => match decode_bounded_script_json(
                            vm,
                            &out.to_string(),
                            BRIDGE_BYTES,
                            BRIDGE_DEPTH,
                        ) {
                            Ok(v) => v,
                            Err(e) => script_err_not_allowed!(
                                vm.bx.threads.cur_ref().trap,
                                "{} result too large: {}",
                                label,
                                e
                            ),
                        },
                        Err(e) => script_err_not_allowed!(
                            vm.bx.threads.cur_ref().trap,
                            "{} refused: {}",
                            label,
                            e
                        ),
                    }
                },
            );
        }
        // Setup-owned: freezing keeps scripts from rewriting a method.
        vm.bx.heap.freeze(module);
    });
}

fn runtime(engine: &SandboxEngine<'_>, body: Option<&str>) -> Result<Runtime, String> {
    let mut rt = Runtime::with_limits((), (), limits()).map_err(|e| e.to_string())?;
    let body: Option<String> = body.map(String::from);
    let select_body = body.clone();
    let hosts = engine.allowed_hosts.to_vec();
    let hosts_req = hosts.clone();
    let allow_http = engine.allow_http;
    install_module(
        &mut rt,
        "net",
        vec![
            (
                "request",
                MAX_REQUESTS,
                Box::new(move |v| request_tool(v, &hosts_req, allow_http)),
            ),
            (
                "url",
                32,
                Box::new(move |v| url_tool(v, &hosts, allow_http)),
            ),
        ],
    );
    install_module(
        &mut rt,
        "markup",
        vec![
            ("feed", 2, Box::new(move |v| feed_tool(v, body.as_deref()))),
            ("text", 4096, Box::new(text_tool)),
            ("matches", 4096, Box::new(matches_tool)),
            (
                "select",
                4,
                Box::new(move |v| select_tool(v, select_body.as_deref())),
            ),
            ("unwrap", 512, Box::new(unwrap_tool)),
        ],
    );
    Ok(rt)
}

/// Evaluate `call` (an expression over the engine's functions and the
/// `input` global) and return its value as JSON.
fn run(
    engine: &SandboxEngine<'_>,
    input: &Value,
    body: Option<&str>,
    call: &str,
) -> Result<Value, String> {
    let mut rt = runtime(engine, body)?;
    rt.set_json_global("input", input, MAX_JSON_BYTES, MAX_JSON_DEPTH)
        .map_err(|e| format!("engine input: {e}"))?;
    let source = format!("{}\n{call}\n", engine.source.trim_end());
    let ev = rt
        .eval(&source)
        .map_err(|e| format!("{}: {e}", engine.id))?;
    if !ev.completed() {
        let diag = ev
            .diagnostics
            .iter()
            .map(|d| strip_source_path(d))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("{}: script failed: {diag}", engine.id));
    }
    rt.script_value_as_json(ev.value, MAX_JSON_BYTES, MAX_JSON_DEPTH)
        .map_err(|e| format!("{}: result is not bounded JSON: {e}", engine.id))
}

/// Diagnostics carry the VM's Rust source location in parentheses; keep
/// only the script-facing part.
fn strip_source_path(d: &str) -> String {
    match d.rfind(" (") {
        Some(i) if d.ends_with(')') && d[i..].contains(".rs:") => d[..i].to_string(),
        _ => d.to_string(),
    }
}

/// Most requests one `build_request` may return.
pub const MAX_REQUESTS: usize = 16;

/// Run `build_request(input.query, input.opts)`. The script returns one
/// request, a list of requests (e.g. one per feed), or `nil` for none.
pub fn build_request(
    engine: &SandboxEngine<'_>,
    query: &str,
    opts: &Value,
) -> Result<Vec<ScriptRequest>, String> {
    let input = json!({ "query": query, "opts": opts });
    let v = run(
        engine,
        &input,
        None,
        "build_request(input.query, input.opts)",
    )?;
    let list = match v {
        Value::Null => Vec::new(),
        Value::Array(a) => a,
        other => vec![other],
    };
    if list.len() > MAX_REQUESTS {
        return Err(format!("{}: more than {MAX_REQUESTS} requests", engine.id));
    }
    list.iter().map(|r| checked_request(engine, r)).collect()
}

fn checked_request(engine: &SandboxEngine<'_>, v: &Value) -> Result<ScriptRequest, String> {
    // Re-check what came back: the script might return a literal record
    // instead of going through net.request.
    let checked = request_tool(v, engine.allowed_hosts, engine.allow_http)
        .map_err(|e| format!("{}: {e}", engine.id))?;
    Ok(ScriptRequest {
        method: checked["method"].as_str().unwrap_or("GET").to_string(),
        url: checked["url"].as_str().unwrap_or_default().to_string(),
        headers: checked["headers"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        body: checked["body"].as_str().map(String::from),
        render: checked["render"].as_bool().unwrap_or(false),
    })
}

/// Run `parse_response(response, opts)` with `response = {url, status, headers,
/// json, body}`: `json` is the decoded body (or `nil`), `body` the raw text
/// when it is not JSON.
pub fn parse_response(
    engine: &SandboxEngine<'_>,
    opts: &Value,
    url: &str,
    status: u16,
    headers: &[(String, String)],
    body: &str,
) -> Result<ScriptParse, String> {
    if body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "{}: response body over {MAX_BODY_BYTES} bytes",
            engine.id
        ));
    }
    let headers: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase().replace('-', "_"), json!(v)))
        .collect();
    // JSON is decoded here, under the host's bounds: OctoScript's own
    // `parse_json` stops at 64 KiB, less than many API responses.
    let data: Option<Value> = serde_json::from_str(body.trim()).ok();
    let is_json = data.is_some();
    let input = json!({
        "response": {
            "url": url,
            "status": status,
            "headers": headers,
            "json": data,
            "body": if is_json { "" } else { body },
        },
        "opts": opts,
    });
    let v = run(
        engine,
        &input,
        Some(body),
        "parse_response(input.response, input.opts)",
    )?;
    let mut challenge = None;
    let (items, backoff, error) = match v {
        Value::Array(items) => (items, None, None),
        Value::Object(mut m) => {
            let error = m
                .get("error")
                .and_then(Value::as_str)
                .map(|e| e.chars().take(300).collect::<String>());
            challenge = m
                .get("challenge")
                .and_then(Value::as_str)
                .map(|e| e.chars().take(200).collect::<String>());
            let items = match m.remove("items") {
                Some(Value::Array(a)) => a,
                Some(Value::Null) | None => Vec::new(),
                Some(_) => return Err(format!("{}: items must be a list", engine.id)),
            };
            let backoff = m
                .get("backoff")
                .and_then(Value::as_f64)
                .filter(|s| s.is_finite() && *s > 0.0)
                .map(|s| Duration::from_secs_f64(s.min(3600.0)));
            (items, backoff, error)
        }
        Value::Null => (Vec::new(), None, None),
        _ => return Err(format!("{}: parse_response must return a list", engine.id)),
    };
    Ok(ScriptParse {
        items,
        backoff,
        error,
        challenge,
    })
}

/// Syntax check plus the two required functions.
pub fn check_engine_source(source: &str) -> Result<(), String> {
    let report = octoscript_core::check_syntax(source).map_err(|e| e.to_string())?;
    if !report.valid {
        let d = report
            .diagnostics
            .iter()
            .map(|d| format!("{}:{}: {}", d.line, d.column, d.message))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("engine.octoscript: {d}"));
    }
    let decls = octoscript_core::top_level_declarations(source).map_err(|e| e.to_string())?;
    for f in ["build_request", "parse_response"] {
        if !decls.iter().any(|d| d.name == f) {
            return Err(format!("engine.octoscript must define fn {f}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine<'a>(source: &'a str, hosts: &'a [String]) -> SandboxEngine<'a> {
        SandboxEngine {
            id: "t",
            source,
            allowed_hosts: hosts,
            allow_http: false,
        }
    }

    const OK: &str = r#"use mod.net
use mod.std.array
use mod.std.object

fn build_request(query, opts) {
    let u = net.url({base: "https://api.example.org/search", query: {q: query, n: opts.count}})
    return net.request({url: u.url, headers: {accept: "application/json"}})
}

fn parse_response(response, opts) {
    let data = response.json
    let out = []
    for h in object.get(data, "hits", []) {
        array.push(out, {url: h.url, title: h.title})
    }
    return {items: out, backoff: object.get(data, "backoff", nil)}
}
"#;

    #[test]
    fn should_build_and_parse_through_the_net_module() {
        let hosts = vec!["api.example.org".to_string()];
        let e = engine(OK, &hosts);
        check_engine_source(OK).unwrap();
        let reqs = build_request(&e, "rust & tokio", &json!({"count": 5})).unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.method, "GET");
        // Query-param order carries no contract: a record query comes back
        // in the host build's serde_json Map order (sorted, or insertion-
        // ordered when the preserve_order feature is unified into the
        // graph), so compare decoded pairs instead of the serialized string.
        assert!(
            req.url.starts_with("https://api.example.org/search?"),
            "{}",
            req.url
        );
        let mut pairs: Vec<(String, String)> = Url::parse(&req.url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("n".into(), "5".into()),
                ("q".into(), "rust & tokio".into())
            ]
        );
        assert_eq!(
            req.headers,
            vec![("accept".into(), "application/json".into())]
        );
        let body = r#"{"hits":[{"url":"https://a.org/1","title":"One"}],"backoff":10}"#;
        let parsed = parse_response(&e, &json!({}), &req.url, 200, &[], body).unwrap();
        assert_eq!(
            parsed.items,
            vec![json!({"url":"https://a.org/1","title":"One"})]
        );
        assert_eq!(parsed.backoff, Some(Duration::from_secs(10)));
    }

    #[test]
    fn should_accept_a_list_of_requests_and_check_each() {
        let hosts = vec!["a.example.org".to_string(), "b.example.org".to_string()];
        let two = "use mod.net\nfn build_request(query, opts) {\nreturn [net.request({url: \"https://a.example.org/rss\"}), net.request({url: \"https://b.example.org/rss\"})]\n}\nfn parse_response(response, opts) {\nreturn [{url: response.url, title: \"t\"}]\n}\n";
        let reqs = build_request(&engine(two, &hosts), "q", &json!({})).unwrap();
        assert_eq!(reqs.len(), 2);
        let parsed = parse_response(
            &engine(two, &hosts),
            &json!({}),
            &reqs[1].url,
            200,
            &[],
            "<rss/>",
        )
        .unwrap();
        assert_eq!(
            parsed.items[0]["url"], "https://b.example.org/rss",
            "parse_response sees which request it answers"
        );

        // A literal list entry on an undeclared host is refused.
        let sneaky = "use mod.net\nfn build_request(query, opts) {\nreturn [net.request({url: \"https://a.example.org/rss\"}), {url: \"https://evil.example.com/\"}]\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(sneaky, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("not declared"), "{err}");
        let none = "fn build_request(query, opts) {\nreturn nil\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        assert!(
            build_request(&engine(none, &hosts), "q", &json!({}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn should_refuse_a_host_outside_the_manifest() {
        let hosts = vec!["api.example.org".to_string()];
        let net_call = "use mod.net\nfn build_request(query, opts) {\nreturn net.request({url: \"https://evil.example.com/x?q=\" + query})\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(net_call, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("not declared in the engine manifest"), "{err}");

        // Returning a literal record without net.request is re-checked.
        let literal = "fn build_request(query, opts) {\nreturn {url: \"https://evil.example.com/\"}\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(literal, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("not declared"), "{err}");

        // Look-alike hosts, ports, credentials and other schemes.
        for bad in [
            "https://api.example.org.evil.com/",
            "https://api.example.org:8443/",
            "https://user@api.example.org/",
            "http://api.example.org/",
            "file:///etc/passwd",
        ] {
            assert!(check_url(bad, &hosts, false).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn should_have_no_tool_module_and_refuse_host_headers() {
        let hosts = vec!["api.example.org".to_string()];
        let tool = "use mod.tool\nfn build_request(query, opts) {\nreturn tool.call(\"shell\", \"id\")\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(tool, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("script failed"), "no tool module: {err}");

        let ua = "use mod.net\nfn build_request(query, opts) {\nreturn net.request({url: \"https://api.example.org/\", headers: {user_agent: \"SomeBrowser/1.0\"}})\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(ua, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("set by the host"), "{err}");
    }

    #[test]
    fn should_stop_runaway_scripts() {
        let hosts = vec!["api.example.org".to_string()];
        let spin = "fn build_request(query, opts) {\nlet i = 0\nwhile true {\ni += 1\n}\nreturn nil\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(spin, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("script failed"), "{err}");
    }

    #[test]
    fn should_stop_scripts_that_exhaust_heap_or_strings() {
        let hosts = vec!["api.example.org".to_string()];
        let grow = "fn build_request(query, opts) {\nlet s = \"xxxxxxxxxxxxxxxx\"\nwhile true {\ns += s\n}\nreturn nil\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(grow, &hosts), "q", &json!({})).unwrap_err();
        assert!(
            err.contains("script failed") && err.contains("limit"),
            "{err}"
        );
    }

    #[test]
    fn should_enforce_host_function_call_budgets_and_bridge_bounds() {
        let hosts = vec!["api.example.org".to_string()];
        // net.url allows 32 calls per run.
        let many = "use mod.net\nuse mod.std.array\nfn build_request(query, opts) {\nlet u = nil\nfor i in array.range(0, 40) {\nu = net.url({base: \"https://api.example.org/\"})\n}\nreturn net.request({url: u.url})\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(many, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("call budget"), "{err}");

        // A record larger than the bridge bound (256 KiB) is refused.
        let big = "use mod.net\nfn build_request(query, opts) {\nlet s = \"xxxxxxxxxxxxxxxx\"\nlet i = 0\nwhile i < 15 {\ns += s\ni += 1\n}\nreturn net.request({url: \"https://api.example.org/\", body: s})\n}\nfn parse_response(response, opts) {\nreturn []\n}\n";
        let err = build_request(&engine(big, &hosts), "q", &json!({})).unwrap_err();
        assert!(err.contains("bounded JSON"), "{err}");
    }

    #[test]
    fn should_unwrap_search_engine_redirect_links() {
        // Observed on each engine's results page.
        assert_eq!(
            unwrap_redirect(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fomarchy.org%2F&rut=eb8d",
                Some("https://html.duckduckgo.com/")
            ),
            "https://omarchy.org/"
        );
        assert_eq!(
            unwrap_redirect(
                "https://www.bing.com/ck/a?!&&p=36454d&u=a1aHR0cHM6Ly9vbWFyY2h5Lm9yZy8&ntb=1",
                None
            ),
            "https://omarchy.org/"
        );
        assert_eq!(
            unwrap_redirect(
                "/url?q=https://example.org/a&sa=U",
                Some("https://www.google.com/")
            ),
            "https://example.org/a"
        );
        assert_eq!(
            unwrap_redirect("https://example.org/x", None),
            "https://example.org/x"
        );
        // A wrapper pointing at a non-http target is left alone.
        assert_eq!(
            unwrap_redirect("https://duckduckgo.com/l/?uddg=javascript%3Aalert(1)", None),
            "https://duckduckgo.com/l/?uddg=javascript%3Aalert(1)"
        );
    }

    #[test]
    fn should_select_fields_from_html_results() {
        let hosts = vec!["a.example.org".to_string()];
        let html = r#"<html><body>
            <div class="result"><a class="t" href="/l/1">One</a><p class="s">first  snippet</p></div>
            <div class="result result--ad"><a class="t" href="/ad">Ad</a></div>
            <div class="result"><a class="t" href="/l/2">Two</a></div>
            <div id="search"><a href="https://g.example/x"><div><h3>Headline</h3></div></a></div>
        </body></html>"#;
        let script = "use mod.markup\nfn build_request(query, opts) {\nreturn nil\n}\nfn parse_response(response, opts) {\nlet a = markup.select({item: \"div.result:not(.result--ad)\", fields: {title: \"a.t\", url: \"a.t@href\", snippet: \"p.s\"}})\nlet b = markup.select({item: \"#search a h3\", fields: {title: \"\", url: \"^a@href\"}})\nreturn [a.items, b.items]\n}\n";
        let parsed = parse_response(
            &engine(script, &hosts),
            &json!({}),
            "https://a.example.org/",
            200,
            &[],
            html,
        )
        .unwrap();
        assert_eq!(
            parsed.items[0],
            json!([
                {"title": "One", "url": "/l/1", "snippet": "first snippet"},
                {"title": "Two", "url": "/l/2", "snippet": null}
            ])
        );
        assert_eq!(
            parsed.items[1],
            json!([{"title": "Headline", "url": "https://g.example/x"}])
        );
    }

    #[test]
    fn should_require_both_engine_functions() {
        assert!(check_engine_source("fn build_request(q, o) {\nreturn nil\n}\n").is_err());
        assert!(check_engine_source("fn build_request(q, o) { return nil }").is_err());
    }
}
