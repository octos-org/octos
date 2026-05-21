// Minimal TOML reader for the M19 UX scenario manifest.
//
// We deliberately avoid adding a TOML dependency. The supported subset is:
//   - line comments starting with `#`
//   - top-level `key = value`
//   - `[[table.array]]` array-of-tables headers
//   - string values (single-line `"..."` with `\"` and `\\` escapes)
//   - integer values
//   - boolean values (`true` / `false`)
//   - inline arrays of strings, possibly multi-line, e.g.
//       arr = [
//         "a",
//         "b",
//       ]
//
// Anything else throws a typed ManifestSchemaError. The manifest in
// `e2e/matrix/octos-ux.toml` is required to stay within this subset; the
// parser self-tests in `e2e/tests/ux/scenario-list.test.mjs` enforce it.

export class ManifestSchemaError extends Error {
  constructor(message, { line, source } = {}) {
    super(message);
    this.name = "ManifestSchemaError";
    this.line = line;
    this.source = source;
  }
}

function stripCommentAndTrim(line) {
  // Strip a `#` comment, but only when it's outside a string literal.
  let inString = false;
  let escape = false;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (escape) {
      escape = false;
      continue;
    }
    if (ch === "\\") {
      escape = true;
      continue;
    }
    if (ch === '"') {
      inString = !inString;
      continue;
    }
    if (ch === "#" && !inString) {
      return line.slice(0, i).trim();
    }
  }
  return line.trim();
}

function parseScalar(raw, lineNo) {
  const trimmed = raw.trim();
  if (trimmed === "") {
    throw new ManifestSchemaError("empty value", { line: lineNo, source: raw });
  }
  if (trimmed === "true") return true;
  if (trimmed === "false") return false;
  if (/^-?\d+$/.test(trimmed)) {
    return Number.parseInt(trimmed, 10);
  }
  if (trimmed.startsWith('"') && trimmed.endsWith('"') && trimmed.length >= 2) {
    return unescapeString(trimmed.slice(1, -1), lineNo);
  }
  throw new ManifestSchemaError(`unsupported scalar value: ${trimmed}`, {
    line: lineNo,
    source: raw,
  });
}

function unescapeString(body, lineNo) {
  let out = "";
  let i = 0;
  while (i < body.length) {
    const ch = body[i];
    if (ch === "\\") {
      const next = body[i + 1];
      if (next === undefined) {
        throw new ManifestSchemaError("dangling escape in string", {
          line: lineNo,
          source: body,
        });
      }
      switch (next) {
        case "\\":
          out += "\\";
          break;
        case '"':
          out += '"';
          break;
        case "n":
          out += "\n";
          break;
        case "t":
          out += "\t";
          break;
        case "r":
          out += "\r";
          break;
        default:
          throw new ManifestSchemaError(
            `unsupported escape sequence \\${next}`,
            { line: lineNo, source: body },
          );
      }
      i += 2;
      continue;
    }
    out += ch;
    i += 1;
  }
  return out;
}

// Parse the right-hand side of `arr = [ "a", "b" ]`, allowing the closing `]`
// to land on a later line. Returns { value, consumed } where `consumed` is the
// number of additional source lines consumed beyond the first.
function parseArray(firstRhs, allLines, startIdx) {
  let buf = firstRhs;
  let consumed = 0;
  while (!bracketBalanced(buf)) {
    const nextIdx = startIdx + 1 + consumed;
    if (nextIdx >= allLines.length) {
      throw new ManifestSchemaError("unterminated array literal", {
        line: startIdx + 1,
        source: firstRhs,
      });
    }
    buf += "\n" + allLines[nextIdx];
    consumed += 1;
  }
  // Strip outer brackets, split on commas at depth 0 (we only support flat
  // arrays of strings/scalars; nested arrays are out of scope).
  const trimmed = buf.trim();
  if (!trimmed.startsWith("[") || !trimmed.endsWith("]")) {
    throw new ManifestSchemaError("malformed array literal", {
      line: startIdx + 1,
      source: buf,
    });
  }
  const inner = trimmed.slice(1, -1);
  const items = splitTopLevelCommas(inner)
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
  const value = items.map((item) => parseScalar(item, startIdx + 1));
  return { value, consumed };
}

function bracketBalanced(s) {
  let depth = 0;
  let inString = false;
  let escape = false;
  for (let i = 0; i < s.length; i += 1) {
    const ch = s[i];
    if (escape) {
      escape = false;
      continue;
    }
    if (ch === "\\") {
      escape = true;
      continue;
    }
    if (ch === '"') {
      inString = !inString;
      continue;
    }
    if (inString) continue;
    if (ch === "[") depth += 1;
    else if (ch === "]") depth -= 1;
  }
  return depth === 0 && !inString;
}

function splitTopLevelCommas(s) {
  const out = [];
  let buf = "";
  let depth = 0;
  let inString = false;
  let escape = false;
  for (let i = 0; i < s.length; i += 1) {
    const ch = s[i];
    if (escape) {
      buf += ch;
      escape = false;
      continue;
    }
    if (ch === "\\") {
      buf += ch;
      escape = true;
      continue;
    }
    if (ch === '"') {
      inString = !inString;
      buf += ch;
      continue;
    }
    if (!inString) {
      if (ch === "[") depth += 1;
      else if (ch === "]") depth -= 1;
      if (ch === "," && depth === 0) {
        out.push(buf);
        buf = "";
        continue;
      }
    }
    buf += ch;
  }
  if (buf.trim().length > 0) out.push(buf);
  return out;
}

// Parse the manifest source text into a typed structure:
//   { top: { ... }, arrays: { "scenario": [ { ... }, ... ] } }
export function parseToml(source) {
  const lines = source.split(/\r?\n/);
  const top = {};
  const arrays = {};
  let currentTable = null; // { kind: "array", name, obj }
  let i = 0;
  while (i < lines.length) {
    const rawLine = lines[i];
    const line = stripCommentAndTrim(rawLine);
    if (line === "") {
      i += 1;
      continue;
    }
    if (line.startsWith("[[") && line.endsWith("]]")) {
      const name = line.slice(2, -2).trim();
      if (!/^[A-Za-z_][A-Za-z0-9_.-]*$/.test(name)) {
        throw new ManifestSchemaError(`invalid table-array header: ${line}`, {
          line: i + 1,
          source: rawLine,
        });
      }
      const obj = {};
      if (!arrays[name]) arrays[name] = [];
      arrays[name].push(obj);
      currentTable = { kind: "array", name, obj };
      i += 1;
      continue;
    }
    if (line.startsWith("[") && line.endsWith("]")) {
      throw new ManifestSchemaError(
        "plain [table] headers not supported; use [[array]]",
        { line: i + 1, source: rawLine },
      );
    }
    const eq = line.indexOf("=");
    if (eq < 0) {
      throw new ManifestSchemaError(`expected key = value, got: ${line}`, {
        line: i + 1,
        source: rawLine,
      });
    }
    const key = line.slice(0, eq).trim();
    const rhs = line.slice(eq + 1).trim();
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
      throw new ManifestSchemaError(`invalid key: ${key}`, {
        line: i + 1,
        source: rawLine,
      });
    }
    let value;
    let advance = 1;
    if (rhs.startsWith("[")) {
      const { value: arr, consumed } = parseArray(rhs, lines, i);
      value = arr;
      advance = 1 + consumed;
    } else {
      value = parseScalar(rhs, i + 1);
    }
    if (currentTable === null) {
      if (Object.prototype.hasOwnProperty.call(top, key)) {
        throw new ManifestSchemaError(`duplicate top-level key: ${key}`, {
          line: i + 1,
        });
      }
      top[key] = value;
    } else {
      if (Object.prototype.hasOwnProperty.call(currentTable.obj, key)) {
        throw new ManifestSchemaError(
          `duplicate key in [[${currentTable.name}]]: ${key}`,
          { line: i + 1 },
        );
      }
      currentTable.obj[key] = value;
    }
    i += advance;
  }
  return { top, arrays };
}
