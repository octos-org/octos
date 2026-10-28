# Built-in Office document generation

The bundled `office` app-skill lets an agent create Word, Excel, and PowerPoint files directly in the current session workspace. It is included in the standard release bundle; no skill installation, Python, Node.js, LibreOffice, network service, or external `mofa-*` tool is required.

## Ask the agent

For example:

- “Create `reports/q3.docx` titled Q3 summary, with two paragraphs, a regional sales table, and `charts/q3.png`.”
- “Create `reports/q3.xlsx` with Summary and Notes sheets. Include a total formula and bold blue headers.”
- “Create `reports/q3-deck.pptx` with two slides, a summary and a regional-sales slide using `charts/q3.png`.”

The agent uses `office_generate` and returns the generated file as an artifact. Paths in the JSON examples below are workspace-relative. An absolute path is accepted only when it is already inside the same workspace. Existing files are never overwritten, so choose a new filename on retry.

## DOCX contract

```json
{
  "kind": "docx",
  "output": "reports/q3.docx",
  "title": "Q3 summary",
  "paragraphs": ["Revenue increased.", "Details are below."],
  "tables": [{"rows": [["Region", "Sales"], ["East", "120"]]}],
  "images": [{"path": "charts/q3.png", "width_inches": 5.5}]
}
```

The generator writes the title, paragraphs, tables, and images in that order. Images must be PNG or JPEG files inside the workspace; width defaults to 6 inches and height preserves the source aspect ratio.

## XLSX contract

```json
{
  "kind": "xlsx",
  "output": "reports/q3.xlsx",
  "sheets": [
    {
      "name": "Summary",
      "rows": [
        [{"value": "Region", "bold": true, "fill": "DDEBF7"}, {"value": "Sales", "bold": true, "fill": "DDEBF7"}],
        ["East", 120],
        [{"value": "Total", "bold": true}, {"formula": "SUM(B2:B3)", "number_format": "#,##0.00"}]
      ]
    },
    {"name": "Notes", "rows": [["Values are in USD."]]}
  ]
}
```

Cells accept plain strings, numbers, and booleans, or an object with `value`, `formula`, `bold`, `fill`, and `number_format`. A formula replaces `value`; the leading `=` is optional. Fills use six-digit RGB values. Workbooks are marked for formula recalculation when opened.

## PPTX contract

```json
{
  "kind": "pptx",
  "output": "reports/q3-deck.pptx",
  "slides": [
    {"title": "Q3 summary", "bullets": ["Revenue increased", "East and West grew"]},
    {"title": "Regional sales", "bullets": ["East: 120", "West: 140"], "image": {"path": "charts/q3.png", "width_inches": 5.5}}
  ]
}
```

The deck contains every requested slide in order. Titles and bullet points remain editable text. An optional PNG or JPEG image is placed to the right of the text with its original aspect ratio.

The complete agent-facing schema and examples are in `crates/app-skills/office/SKILL.md` and `manifest.json`. Generation uses Rust `zip`, `quick-xml`, and `image`; the skill performs no network requests and does not execute install or lifecycle scripts.
