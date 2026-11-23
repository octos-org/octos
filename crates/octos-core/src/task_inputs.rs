//! Inputs a task declares for its first model request.
//!
//! A task — a fleet task, a peer handoff — can name the files its worker
//! needs to begin: source text, structured data, images. The runtime puts
//! them into the worker's FIRST request to its model, so the worker does not
//! spend its opening turns reading files it was always going to read.
//!
//! This module holds the declaration and the delivery report; both are plain
//! data that a plan persists and a host reads back. Reading the files is
//! `octos_agent::task_inputs`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The most inputs one task may declare.
pub const MAX_DECLARED_INPUTS: usize = 32;

/// One file a task declares it needs in its first request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredInput {
    /// The file, as the worker would name it to `read_file`: relative to the
    /// worker's working directory, or absolute where the worker may read it.
    pub path: String,
    /// How to deliver it. [`InputKind::Auto`] decides from the file itself.
    #[serde(default, skip_serializing_if = "InputKind::is_auto")]
    pub kind: InputKind,
    /// A required input that cannot be delivered whole stops the task before
    /// its first model call. An optional one is left out and reported.
    #[serde(default = "required_by_default", skip_serializing_if = "is_true")]
    pub required: bool,
    /// The content this declaration means, as lowercase hex SHA-256. When
    /// set, a file whose content differs is not delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

fn required_by_default() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl DeclaredInput {
    /// A required input, delivered in whatever form the file calls for.
    pub fn required(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: InputKind::Auto,
            required: true,
            sha256: None,
        }
    }

    /// An input the task can start without.
    pub fn optional(path: impl Into<String>) -> Self {
        Self {
            required: false,
            ..Self::required(path)
        }
    }
}

/// The form a task asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// Decide from the file: a recognised image is an image, a data format is
    /// data, any other UTF-8 file is text.
    #[default]
    Auto,
    /// UTF-8 text, verbatim.
    Text,
    /// Structured data, verbatim. JSON and JSON Lines must parse.
    Data,
    /// The image itself, for a model that accepts images.
    Image,
}

impl InputKind {
    fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// The form an input was delivered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputForm {
    Text,
    Data,
    Image,
}

impl InputForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Data => "data",
            Self::Image => "image",
        }
    }
}

/// Why an input is not in the first request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmitReason {
    /// No file at that path.
    Missing,
    /// The worker may not read that path.
    Denied,
    /// The content is not what the declaration pinned.
    Changed,
    /// It does not fit the room the first request has for inputs.
    OverBudget,
    /// The model, or this runtime, cannot take a file of this form.
    Unsupported,
    /// The file is there and allowed, and could not be read.
    Unreadable,
    /// Declared as data, and it does not parse.
    Invalid,
    /// The same file is declared earlier in the list.
    Duplicate,
}

impl OmitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Denied => "denied",
            Self::Changed => "changed",
            Self::OverBudget => "over budget",
            Self::Unsupported => "unsupported",
            Self::Unreadable => "unreadable",
            Self::Invalid => "invalid",
            Self::Duplicate => "duplicate",
        }
    }
}

/// What became of one declared input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InputOutcome {
    /// The whole file is in the first request.
    Delivered {
        form: InputForm,
        bytes: u64,
        sha256: String,
        /// Estimated cost in the request, in tokens.
        tokens: u32,
    },
    /// None of the file is in the first request.
    Omitted { reason: OmitReason, detail: String },
}

/// One declared input and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRecord {
    /// The path as declared.
    pub path: String,
    pub required: bool,
    #[serde(flatten)]
    pub outcome: InputOutcome,
}

impl InputRecord {
    pub fn is_delivered(&self) -> bool {
        matches!(self.outcome, InputOutcome::Delivered { .. })
    }
}

/// What a first request carries of the inputs its task declared: one record
/// per declaration, in declaration order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputReport {
    pub inputs: Vec<InputRecord>,
    /// The room the first request had for inputs, in estimated tokens.
    pub budget_tokens: u32,
    /// How much of it the delivered inputs take.
    pub used_tokens: u32,
}

impl InputReport {
    /// Required inputs that are not in the request. A repeated declaration of
    /// a file is not one of them: the first declaration carries that file's
    /// outcome, and is required if any declaration of the file is.
    pub fn undelivered_required(&self) -> impl Iterator<Item = &InputRecord> {
        self.inputs.iter().filter(|record| {
            record.required
                && !matches!(
                    record.outcome,
                    InputOutcome::Delivered { .. }
                        | InputOutcome::Omitted {
                            reason: OmitReason::Duplicate,
                            ..
                        }
                )
        })
    }

    pub fn delivered(&self) -> impl Iterator<Item = &InputRecord> {
        self.inputs.iter().filter(|record| record.is_delivered())
    }

    pub fn omitted(&self) -> impl Iterator<Item = &InputRecord> {
        self.inputs.iter().filter(|record| !record.is_delivered())
    }

    /// One line per input, for a log or a tool result.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for record in &self.inputs {
            if !out.is_empty() {
                out.push('\n');
            }
            match &record.outcome {
                InputOutcome::Delivered {
                    form,
                    bytes,
                    sha256,
                    ..
                } => out.push_str(&format!(
                    "{}: delivered as {} ({bytes} bytes, sha256 {})",
                    record.path,
                    form.as_str(),
                    short_digest(sha256),
                )),
                InputOutcome::Omitted { reason, detail } => out.push_str(&format!(
                    "{}: not delivered ({}, {}): {detail}",
                    record.path,
                    if record.required {
                        "required"
                    } else {
                        "optional"
                    },
                    reason.as_str(),
                )),
            }
        }
        out
    }
}

/// The first twelve hex digits of a digest: enough to tell two contents
/// apart in a line a person reads.
pub fn short_digest(sha256: &str) -> &str {
    sha256.get(..12).unwrap_or(sha256)
}

/// Read a tool argument that declares inputs: absent or `null` declares
/// none; otherwise an array whose items are a path, or an object with `path`
/// and optional `kind`, `required` and `sha256`.
pub fn parse_declared_inputs(value: Option<&Value>) -> Result<Vec<DeclaredInput>, String> {
    let items = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err("inputs must be an array".to_owned()),
    };
    if items.len() > MAX_DECLARED_INPUTS {
        return Err(format!(
            "inputs lists {} files; at most {MAX_DECLARED_INPUTS} may be declared",
            items.len()
        ));
    }
    let mut inputs = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let input = match item {
            Value::String(path) => DeclaredInput::required(path.as_str()),
            Value::Object(_) => serde_json::from_value::<StrictInput>(item.clone())
                .map(DeclaredInput::from)
                .map_err(|err| format!("inputs[{index}]: {err}"))?,
            _ => {
                return Err(format!(
                    "inputs[{index}] must be a path or an object with a path"
                ));
            }
        };
        if input.path.trim().is_empty() || input.path.contains('\0') {
            return Err(format!("inputs[{index}] has no usable path"));
        }
        if let Some(digest) = &input.sha256 {
            let hex = digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex {
                return Err(format!(
                    "inputs[{index}].sha256 must be 64 lowercase hex digits"
                ));
            }
        }
        inputs.push(input);
    }
    Ok(inputs)
}

/// The object form of a declaration as a tool argument. Unlike the persisted
/// [`DeclaredInput`] it refuses unknown fields, so a misspelt `required` is
/// an error instead of a silently required input.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictInput {
    path: String,
    #[serde(default)]
    kind: InputKind,
    #[serde(default = "required_by_default")]
    required: bool,
    #[serde(default)]
    sha256: Option<String>,
}

impl From<StrictInput> for DeclaredInput {
    fn from(input: StrictInput) -> Self {
        Self {
            path: input.path,
            kind: input.kind,
            required: input.required,
            sha256: input.sha256,
        }
    }
}

/// The JSON schema of an `inputs` tool argument. Every tool that lets a task
/// declare inputs uses this one schema, so a declaration reads the same
/// wherever it is made.
pub fn declared_inputs_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_DECLARED_INPUTS,
        "description": "Files the worker needs to begin. The runtime puts them into the worker's FIRST model request — text and data verbatim, images as images — so the worker does not spend turns reading them. Declare what the task cannot start without; a path alone is a required input. Only files the worker itself may read are delivered. A required input that is missing, not readable, changed, or too large for the model's context is never cut short or sent in part: the task or handoff is refused with a report of every input, or the worker is told that none were delivered.",
        "items": {
            "oneOf": [
                { "type": "string", "description": "Path of a required input." },
                {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The file, relative to the worker's working directory, or absolute."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["auto", "text", "data", "image"],
                            "description": "How to deliver it. Default auto: decided from the file. An image is a PNG, JPEG or WebP of at most 5 MiB; downscale or convert a larger one first."
                        },
                        "required": {
                            "type": "boolean",
                            "description": "Default true. false: leave it out and say so when it cannot be delivered."
                        },
                        "sha256": {
                            "type": "string",
                            "description": "Lowercase hex SHA-256 of the content meant. A file with other content is not delivered."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_a_bare_path_as_a_required_input_when_parsing_a_declaration() {
        let inputs = parse_declared_inputs(Some(&json!([
            "src/lib.rs",
            { "path": "page.png", "kind": "image", "required": false },
        ])))
        .unwrap();

        assert_eq!(inputs[0], DeclaredInput::required("src/lib.rs"));
        assert_eq!(
            inputs[1],
            DeclaredInput {
                kind: InputKind::Image,
                ..DeclaredInput::optional("page.png")
            }
        );
        assert_eq!(parse_declared_inputs(None).unwrap(), vec![]);
        assert_eq!(parse_declared_inputs(Some(&Value::Null)).unwrap(), vec![]);
    }

    #[test]
    fn should_refuse_a_declaration_when_it_is_malformed() {
        let too_many = Value::Array(vec![json!("a"); MAX_DECLARED_INPUTS + 1]);
        for (bad, names) in [
            (json!("src/lib.rs"), "array"),
            (json!([42]), "inputs[0]"),
            (json!(["ok", "  "]), "inputs[1]"),
            (json!([{ "kind": "text" }]), "inputs[0]"),
            (json!([{ "path": "a", "requird": false }]), "inputs[0]"),
            (json!([{ "path": "a", "kind": "video" }]), "inputs[0]"),
            (json!([{ "path": "a", "sha256": "ABC" }]), "sha256"),
            (too_many, "at most"),
        ] {
            let err = parse_declared_inputs(Some(&bad)).unwrap_err();
            assert!(err.contains(names), "{bad}: {err}");
        }
    }

    #[test]
    fn should_persist_a_plain_required_input_as_its_path_alone() {
        let plain = serde_json::to_value(DeclaredInput::required("a.txt")).unwrap();
        assert_eq!(plain, json!({ "path": "a.txt" }));

        let pinned = DeclaredInput {
            kind: InputKind::Data,
            sha256: Some("0".repeat(64)),
            ..DeclaredInput::optional("b.json")
        };
        let round_trip: DeclaredInput =
            serde_json::from_value(serde_json::to_value(&pinned).unwrap()).unwrap();
        assert_eq!(round_trip, pinned);
        // A record written before a field existed still reads.
        let old: DeclaredInput = serde_json::from_value(json!({ "path": "c" })).unwrap();
        assert_eq!(old, DeclaredInput::required("c"));
    }

    #[test]
    fn should_name_every_input_and_its_reason_in_the_report_summary() {
        let report = InputReport {
            inputs: vec![
                InputRecord {
                    path: "a.rs".into(),
                    required: true,
                    outcome: InputOutcome::Delivered {
                        form: InputForm::Text,
                        bytes: 12,
                        sha256: "abcdef0123456789".into(),
                        tokens: 3,
                    },
                },
                InputRecord {
                    path: "b.png".into(),
                    required: true,
                    outcome: InputOutcome::Omitted {
                        reason: OmitReason::Missing,
                        detail: "no such file".into(),
                    },
                },
                InputRecord {
                    path: "c.md".into(),
                    required: false,
                    outcome: InputOutcome::Omitted {
                        reason: OmitReason::OverBudget,
                        detail: "needs 900 tokens; 10 left".into(),
                    },
                },
                InputRecord {
                    path: "./a.rs".into(),
                    required: true,
                    outcome: InputOutcome::Omitted {
                        reason: OmitReason::Duplicate,
                        detail: "the same file as input 1 (`a.rs`)".into(),
                    },
                },
            ],
            budget_tokens: 100,
            used_tokens: 3,
        };

        assert_eq!(
            report.summary(),
            "a.rs: delivered as text (12 bytes, sha256 abcdef012345)\n\
             b.png: not delivered (required, missing): no such file\n\
             c.md: not delivered (optional, over budget): needs 900 tokens; 10 left\n\
             ./a.rs: not delivered (required, duplicate): the same file as input 1 (`a.rs`)"
        );
        // A file declared twice does not block on its second declaration.
        let blocking: Vec<_> = report.undelivered_required().map(|r| &r.path).collect();
        assert_eq!(blocking, ["b.png"]);
        assert_eq!(report.delivered().count(), 1);
        assert_eq!(report.omitted().count(), 3);
        // The report is what a host reads back: it must survive a round trip.
        let wire = serde_json::to_value(&report).unwrap();
        assert_eq!(wire["inputs"][1]["status"], "omitted");
        assert_eq!(wire["inputs"][1]["reason"], "missing");
        assert_eq!(serde_json::from_value::<InputReport>(wire).unwrap(), report);
    }
}
