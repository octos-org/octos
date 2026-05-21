// M19 UX scenario manifest loader and runnability classifier.
//
// Entrypoints:
//   loadManifest({ path }) -> { schemaVersion, pack, owner, scenarios }
//   classifyRunnability(scenario, env) -> "runnable" | "skipped" | "blocked" | "quarantined"
//   filterByTier(scenarios, tier) -> Scenario[]
//
// The classifier does NOT spawn tmux, run octos, or read any process. It only
// inspects:
//   - the requested tier (env.tier)
//   - whether the scenario is marked `quarantine = true` in the manifest
//   - whether host tools exist (`env.toolExists("tmux")` etc.)
//   - whether required env vars are set (`env.envHas("OPENAI_API_KEY")`)
//   - whether required capabilities are declared in `env.knownCapabilities`
//
// All inputs are injected so the function stays pure and testable.

import { readFileSync } from "node:fs";
import { parseToml, ManifestSchemaError } from "./toml-lite.mjs";

export { ManifestSchemaError } from "./toml-lite.mjs";

const REQUIRED_SCENARIO_FIELDS = [
  "id",
  "title",
  "description",
  "tier",
  "transport",
  "provider",
  "terminal",
  "tui_binary",
  "tmux_command",
  "required_tools",
  "required_capabilities",
  "expected_artifacts",
  "acceptance",
];

const VALID_TIERS = new Set(["fast", "local", "release"]);
const VALID_TRANSPORTS = new Set(["stdio", "ws"]);
const VALID_PROVIDERS = new Set(["fixture", "live", "none"]);

export const TIER_ORDER = ["fast", "local", "release"];

function tierIncludes(filter, scenarioTier) {
  // `fast` runs in any tier; `local` runs in local/release; `release` runs
  // only in release. Higher tiers are supersets.
  const idxFilter = TIER_ORDER.indexOf(filter);
  const idxScenario = TIER_ORDER.indexOf(scenarioTier);
  return idxScenario <= idxFilter;
}

export function loadManifest({ path }) {
  const source = readFileSync(path, "utf8");
  let parsed;
  try {
    parsed = parseToml(source);
  } catch (err) {
    if (err instanceof ManifestSchemaError) throw err;
    throw new ManifestSchemaError(`parse error: ${err.message}`);
  }
  const { top, arrays } = parsed;
  const schemaVersion = top.schema_version;
  if (typeof schemaVersion !== "number") {
    throw new ManifestSchemaError(
      `missing or non-integer top-level schema_version`,
    );
  }
  if (schemaVersion !== 1) {
    throw new ManifestSchemaError(
      `unsupported schema_version: ${schemaVersion} (expected 1)`,
    );
  }
  if (typeof top.pack !== "string" || top.pack.length === 0) {
    throw new ManifestSchemaError(`missing top-level "pack"`);
  }
  if (typeof top.owner !== "string" || top.owner.length === 0) {
    throw new ManifestSchemaError(`missing top-level "owner"`);
  }
  const scenarioRows = arrays.scenario ?? [];
  if (scenarioRows.length === 0) {
    throw new ManifestSchemaError(`manifest has no [[scenario]] entries`);
  }
  const seenIds = new Set();
  const scenarios = scenarioRows.map((row, idx) => normalizeScenario(row, idx, seenIds));
  return {
    schemaVersion,
    pack: top.pack,
    owner: top.owner,
    scenarios,
  };
}

// Codex P2 follow-up: scalar fields that downstream code (CLI,
// runner, table renderer, JSON consumers) assume are non-empty
// strings. The earlier check only verified presence (i.e. that the
// TOML key existed at all), so a value like `42` or `[]` or `""`
// would slip through and later crash with a confusing error.
const REQUIRED_NON_EMPTY_STRING_FIELDS = [
  "title",
  "description",
  "terminal",
  "tui_binary",
  "tmux_command",
];

function normalizeScenario(row, idx, seenIds) {
  for (const field of REQUIRED_SCENARIO_FIELDS) {
    if (!Object.prototype.hasOwnProperty.call(row, field)) {
      throw new ManifestSchemaError(
        `scenario #${idx + 1} is missing required field "${field}"`,
      );
    }
  }
  for (const field of REQUIRED_NON_EMPTY_STRING_FIELDS) {
    if (
      !Object.prototype.hasOwnProperty.call(row, field) ||
      typeof row[field] !== "string" ||
      row[field].length === 0
    ) {
      throw new ManifestSchemaError(
        `scenario #${idx + 1} has invalid "${field}": must be a non-empty string`,
      );
    }
  }
  if (typeof row.id !== "string" || !/^[a-z][a-z0-9-]*$/.test(row.id)) {
    throw new ManifestSchemaError(
      `scenario #${idx + 1} has invalid id "${row.id}" (kebab-case required)`,
    );
  }
  if (seenIds.has(row.id)) {
    throw new ManifestSchemaError(`duplicate scenario id: ${row.id}`);
  }
  seenIds.add(row.id);
  if (!VALID_TIERS.has(row.tier)) {
    throw new ManifestSchemaError(
      `scenario "${row.id}" has invalid tier "${row.tier}"`,
    );
  }
  if (!VALID_TRANSPORTS.has(row.transport)) {
    throw new ManifestSchemaError(
      `scenario "${row.id}" has invalid transport "${row.transport}"`,
    );
  }
  if (!VALID_PROVIDERS.has(row.provider)) {
    throw new ManifestSchemaError(
      `scenario "${row.id}" has invalid provider "${row.provider}"`,
    );
  }
  // Codex P2 follow-up: optional scalar fields (replay, notes) default
  // to null when absent. But if the TOML sets them to a non-string (e.g.
  // `replay = []` or `notes = 42`), the value still slips through and
  // ends up in the --json projection — breaking the README contract
  // that says these are string-or-null. Validate the *type* when set,
  // but allow empty strings: the v1 contract is "string-or-null" and
  // tightening to "non-empty string" would be a schema-breaking change
  // without a schema_version bump.
  for (const optField of ["replay", "notes"]) {
    if (
      Object.prototype.hasOwnProperty.call(row, optField) &&
      row[optField] !== null &&
      row[optField] !== undefined
    ) {
      if (typeof row[optField] !== "string") {
        throw new ManifestSchemaError(
          `scenario "${row.id}" field "${optField}" must be a string or null`,
        );
      }
    }
  }
  for (const arrField of [
    "required_tools",
    "required_capabilities",
    "expected_artifacts",
    "acceptance",
  ]) {
    if (!Array.isArray(row[arrField])) {
      throw new ManifestSchemaError(
        `scenario "${row.id}" field "${arrField}" must be an array`,
      );
    }
    for (const item of row[arrField]) {
      if (typeof item !== "string" || item.length === 0) {
        throw new ManifestSchemaError(
          `scenario "${row.id}" field "${arrField}" must contain non-empty strings`,
        );
      }
    }
  }
  return {
    id: row.id,
    title: row.title,
    description: row.description,
    tier: row.tier,
    transport: row.transport,
    provider: row.provider,
    terminal: row.terminal,
    tuiBinary: row.tui_binary,
    tmuxCommand: row.tmux_command,
    requiredTools: row.required_tools,
    requiredCapabilities: row.required_capabilities,
    expectedArtifacts: row.expected_artifacts,
    acceptance: row.acceptance,
    replay: row.replay ?? null,
    notes: row.notes ?? null,
    quarantine: row.quarantine === true,
  };
}

export function filterByTier(scenarios, tier) {
  if (!VALID_TIERS.has(tier)) {
    throw new Error(
      `invalid tier "${tier}" (valid: ${[...VALID_TIERS].join(", ")})`,
    );
  }
  return scenarios.filter((s) => tierIncludes(tier, s.tier));
}

// Returns { status, reasons[] } where status is one of:
//   "runnable"    - all gates green
//   "skipped"     - host requirement missing (tool / env)
//   "blocked"     - capability missing in current AppUI surface
//   "quarantined" - manifest marks scenario as quarantined
export function classifyRunnability(scenario, env) {
  const reasons = [];
  if (scenario.quarantine) {
    reasons.push("scenario marked quarantine=true in manifest");
    return { status: "quarantined", reasons };
  }
  for (const tool of scenario.requiredTools) {
    if (!env.toolExists(tool)) {
      reasons.push(`required tool not on PATH: ${tool}`);
    }
  }
  if (scenario.provider === "live") {
    // Live provider scenarios need at least one provider key. In M19-A we
    // don't yet enumerate which one; the runner (#1065) will refine this.
    if (
      !env.envHas("OPENAI_API_KEY") &&
      !env.envHas("ANTHROPIC_API_KEY") &&
      !env.envHas("GOOGLE_API_KEY") &&
      !env.envHas("OPENROUTER_API_KEY")
    ) {
      reasons.push("live provider requires at least one provider API key");
    }
  }
  if (reasons.length > 0) {
    return { status: "skipped", reasons };
  }
  const missingCaps = scenario.requiredCapabilities.filter(
    (cap) => !env.knownCapabilities.has(cap),
  );
  if (missingCaps.length > 0) {
    return {
      status: "blocked",
      reasons: missingCaps.map(
        (cap) => `required capability not advertised by AppUI: ${cap}`,
      ),
    };
  }
  return { status: "runnable", reasons: [] };
}
