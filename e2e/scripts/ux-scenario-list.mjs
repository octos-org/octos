#!/usr/bin/env node
// M19-A: ux:scenario:list
//
// Reads e2e/matrix/octos-ux.toml and prints the declared UX scenarios.
// Does NOT launch tmux, octos, or any backend. The list command must work
// even when the host has no octos binary installed.
//
// Usage:
//   npm --prefix e2e run ux:scenario:list                # full release tier
//   npm --prefix e2e run ux:scenario:list -- --tier fast
//   npm --prefix e2e run ux:scenario:list -- --tier local
//   npm --prefix e2e run ux:scenario:list -- --json
//
// Exit codes:
//   0  - manifest parsed and printed successfully
//   2  - usage error (unknown flag, bad tier)
//   3  - manifest schema error

import { execSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  classifyRunnability,
  filterByTier,
  loadManifest,
  ManifestSchemaError,
  TIER_ORDER,
} from "../lib/ux/scenarios.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const REPO_ROOT = resolve(__dirname, "..", "..");
const DEFAULT_MANIFEST = resolve(REPO_ROOT, "e2e", "matrix", "octos-ux.toml");

function parseArgs(argv) {
  const opts = { tier: "release", json: false, manifest: DEFAULT_MANIFEST };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--tier") {
      opts.tier = argv[i + 1];
      i += 1;
    } else if (arg.startsWith("--tier=")) {
      opts.tier = arg.slice("--tier=".length);
    } else if (arg === "--json") {
      opts.json = true;
    } else if (arg === "--manifest") {
      opts.manifest = resolve(argv[i + 1]);
      i += 1;
    } else if (arg.startsWith("--manifest=")) {
      opts.manifest = resolve(arg.slice("--manifest=".length));
    } else if (arg === "--help" || arg === "-h") {
      opts.help = true;
    } else {
      throw new UsageError(`unknown argument: ${arg}`);
    }
  }
  if (!TIER_ORDER.includes(opts.tier)) {
    throw new UsageError(
      `--tier must be one of ${TIER_ORDER.join(", ")} (got "${opts.tier}")`,
    );
  }
  return opts;
}

class UsageError extends Error {}

function usage() {
  return [
    "Usage: ux:scenario:list [--tier fast|local|release] [--json] [--manifest path]",
    "",
    "Lists UX scenarios declared in e2e/matrix/octos-ux.toml without launching",
    "tmux or any backend. See e2e/ux/README.md for the manifest schema.",
  ].join("\n");
}

function makeEnv() {
  const knownCapabilities = new Set();
  const capsPath = resolve(REPO_ROOT, "e2e", "matrix", "ux-capabilities.json");
  if (existsSync(capsPath)) {
    try {
      const parsed = JSON.parse(readFileSync(capsPath, "utf8"));
      if (Array.isArray(parsed.capabilities)) {
        for (const cap of parsed.capabilities) {
          if (typeof cap === "string") knownCapabilities.add(cap);
        }
      }
    } catch {
      // Best-effort; fall back to empty capability set.
    }
  }
  return {
    toolExists(name) {
      try {
        execSync(`command -v ${shellEscape(name)}`, { stdio: "ignore" });
        return true;
      } catch {
        return false;
      }
    },
    envHas(name) {
      return typeof process.env[name] === "string" && process.env[name].length > 0;
    },
    knownCapabilities,
  };
}

function shellEscape(s) {
  return `'${s.replace(/'/g, "'\\''")}'`;
}

function padRight(s, width) {
  if (s.length >= width) return s;
  return s + " ".repeat(width - s.length);
}

function formatTable(rows, columns) {
  const widths = columns.map((col) =>
    Math.max(col.header.length, ...rows.map((r) => String(r[col.key] ?? "").length)),
  );
  const lines = [];
  const headerLine = columns
    .map((col, i) => padRight(col.header, widths[i]))
    .join("  ");
  lines.push(headerLine);
  lines.push(columns.map((_, i) => "-".repeat(widths[i])).join("  "));
  for (const r of rows) {
    lines.push(
      columns
        .map((col, i) => padRight(String(r[col.key] ?? ""), widths[i]))
        .join("  "),
    );
  }
  return lines.join("\n");
}

function summarize(rows) {
  const counts = { runnable: 0, skipped: 0, blocked: 0, quarantined: 0 };
  for (const r of rows) {
    counts[r.status] = (counts[r.status] ?? 0) + 1;
  }
  return counts;
}

function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (err) {
    if (err instanceof UsageError) {
      process.stderr.write(`error: ${err.message}\n\n${usage()}\n`);
      process.exit(2);
    }
    throw err;
  }
  if (opts.help) {
    process.stdout.write(usage() + "\n");
    process.exit(0);
  }
  let manifest;
  try {
    manifest = loadManifest({ path: opts.manifest });
  } catch (err) {
    if (err instanceof ManifestSchemaError) {
      process.stderr.write(`manifest schema error: ${err.message}\n`);
      if (err.line) {
        process.stderr.write(`  at line ${err.line}\n`);
      }
      process.exit(3);
    }
    throw err;
  }
  const env = makeEnv();
  const filtered = filterByTier(manifest.scenarios, opts.tier);
  // Sort by id for deterministic output.
  const sorted = [...filtered].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  const rows = sorted.map((s) => {
    const { status, reasons } = classifyRunnability(s, env);
    // Codex P2 follow-up: emit the FULL normalized scenario record in
    // each row so JSON consumers (CI, the future runner in #1064-#1067)
    // can decide which TUI binary / tmux command / replay payload /
    // notes to use without reparsing the TOML. Previously the JSON
    // output was just the table-column projection (id/tier/transport/
    // provider/terminal/title), missing description/tuiBinary/
    // tmuxCommand/replay/notes/quarantine.
    return {
      id: s.id,
      tier: s.tier,
      transport: s.transport,
      provider: s.provider,
      terminal: s.terminal,
      title: s.title,
      description: s.description,
      tuiBinary: s.tuiBinary,
      tmuxCommand: s.tmuxCommand,
      status,
      reasons,
      requiredTools: s.requiredTools,
      requiredCapabilities: s.requiredCapabilities,
      acceptance: s.acceptance,
      expectedArtifacts: s.expectedArtifacts,
      replay: s.replay,
      notes: s.notes,
      quarantine: s.quarantine,
    };
  });
  const summary = summarize(rows);
  if (opts.json) {
    process.stdout.write(
      JSON.stringify(
        {
          schema_version: manifest.schemaVersion,
          pack: manifest.pack,
          owner: manifest.owner,
          tier_filter: opts.tier,
          summary,
          scenarios: rows,
        },
        null,
        2,
      ) + "\n",
    );
  } else {
    process.stdout.write(
      `Pack: ${manifest.pack}  Owner: ${manifest.owner}  Tier filter: ${opts.tier}\n`,
    );
    process.stdout.write(
      formatTable(rows, [
        { key: "id", header: "id" },
        { key: "transport", header: "transport" },
        { key: "tier", header: "tier" },
        { key: "status", header: "status" },
        { key: "title", header: "title" },
      ]) + "\n",
    );
    process.stdout.write(
      `\nSummary: runnable=${summary.runnable ?? 0} skipped=${summary.skipped ?? 0} blocked=${summary.blocked ?? 0} quarantined=${summary.quarantined ?? 0}\n`,
    );
    const withReasons = rows.filter((r) => r.reasons && r.reasons.length > 0);
    if (withReasons.length > 0) {
      process.stdout.write(`\nReasons:\n`);
      for (const r of withReasons) {
        for (const reason of r.reasons) {
          process.stdout.write(`  [${r.status}] ${r.id}: ${reason}\n`);
        }
      }
    }
  }
}

main();
