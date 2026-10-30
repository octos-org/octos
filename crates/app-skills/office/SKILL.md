---
name: office
description: Generate Word DOCX and Excel XLSX files locally. Use for Word documents, reports with tables or images, spreadsheets, workbooks, formulas, and formatted cells. No external runtime or network is needed.
version: 1.0.0
author: octos
always: false
---

# Office document generation

Call `office_generate` once per output file. Paths are relative to the session workspace. Images must be PNG or JPEG already inside that workspace. The tool never overwrites an existing file; choose a new name.

## DOCX

```json
{
  "kind": "docx",
  "output": "reports/q3-summary.docx",
  "title": "Q3 summary",
  "paragraphs": ["Revenue increased.", "Details are below."],
  "tables": [{"rows": [["Region", "Sales"], ["East", "120"], ["West", "140"]]}],
  "images": [{"path": "charts/q3.png", "width_inches": 5.5}]
}
```

The document contains the title, paragraphs, every table row/cell, and embedded images in that order.

## XLSX

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
        ["West", 140],
        [{"value": "Total", "bold": true}, {"formula": "SUM(B2:B3)", "number_format": "#,##0.00"}]
      ]
    },
    {"name": "Notes", "rows": [["Values are in USD."]]}
  ]
}
```

A cell can be a JSON string, number, or boolean, or an object with:

- `value`: string, number, or boolean;
- `formula`: formula text, with or without the leading `=` (use instead of `value`);
- `bold`: `true` for bold text;
- `fill`: six-digit RGB fill such as `DDEBF7`;
- `number_format`: Excel number format such as `0.00`, `0%`, or `#,##0.00`.

Formulas are marked for recalculation when the workbook opens. No LibreOffice or other external program is required.
