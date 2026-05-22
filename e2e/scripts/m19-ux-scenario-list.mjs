#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '..', '..');
const defaultManifestPath = path.join(repoRoot, 'e2e', 'matrix', 'octos-ux.toml');
const siblingOctosTuiRepo = path.resolve(repoRoot, '..', 'octos-tui');
const statusClasses = ['runnable', 'skipped', 'blocked', 'quarantined'];
const initialM19ScenarioIds = [
  'stdio-happy-path',
  'websocket-happy-path',
  'tui-solo-onboarding',
  'provider-missing-recoverable',
  'permission-selection',
  'approval-denial',
  'task-subagent-tree',
  'restart-reconnect',
  'narrow-layout',
  'dropped-completion-backpressure',
  'router-status-failover',
];

function usage() {
  console.log(`Usage: node scripts/m19-ux-scenario-list.mjs [--manifest <path>] [--json]

Print the M19 UX scenario matrix from e2e/matrix/octos-ux.toml.

The command only reads the manifest and checks host-tool availability. It does
not launch tmux, the Octos backend, or any scenario runner.`);
}

function parseArgs(argv) {
  const args = { json: false, help: false, manifest: null };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--json') {
      args.json = true;
    } else if (arg === '--help' || arg === '-h') {
      args.help = true;
    } else if (arg === '--manifest') {
      const value = argv[i + 1];
      if (!value || value.startsWith('--')) {
        throw new Error('--manifest requires a path');
      }
      args.manifest = value;
      i++;
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return args;
}

function stripInlineComment(line) {
  let quote = null;
  let escaped = false;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (quote === '"') {
      if (escaped) {
        escaped = false;
      } else if (ch === '\\') {
        escaped = true;
      } else if (ch === '"') {
        quote = null;
      }
      continue;
    }
    if (quote === "'") {
      if (ch === "'") quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (ch === '#') {
      return line.slice(0, i);
    }
  }
  return line;
}

function findKeyValueSeparator(line) {
  let quote = null;
  let escaped = false;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (quote === '"') {
      if (escaped) {
        escaped = false;
      } else if (ch === '\\') {
        escaped = true;
      } else if (ch === '"') {
        quote = null;
      }
      continue;
    }
    if (quote === "'") {
      if (ch === "'") quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (ch === '=') {
      return i;
    }
  }
  return -1;
}

function splitArrayItems(inner) {
  const items = [];
  let quote = null;
  let escaped = false;
  let depth = 0;
  let start = 0;
  for (let i = 0; i < inner.length; i++) {
    const ch = inner[i];
    if (quote === '"') {
      if (escaped) {
        escaped = false;
      } else if (ch === '\\') {
        escaped = true;
      } else if (ch === '"') {
        quote = null;
      }
      continue;
    }
    if (quote === "'") {
      if (ch === "'") quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (ch === '[') {
      depth++;
    } else if (ch === ']') {
      depth--;
    } else if (ch === ',' && depth === 0) {
      items.push(inner.slice(start, i).trim());
      start = i + 1;
    }
  }
  const last = inner.slice(start).trim();
  if (last) items.push(last);
  return items;
}

function parseTomlValue(rawValue) {
  const value = rawValue.trim();
  if (value.startsWith('"')) {
    return JSON.parse(value);
  }
  if (value.startsWith("'")) {
    if (!value.endsWith("'")) throw new Error(`unterminated literal string: ${value}`);
    return value.slice(1, -1);
  }
  if (value.startsWith('[')) {
    if (!value.endsWith(']')) throw new Error(`unterminated array: ${value}`);
    const inner = value.slice(1, -1).trim();
    if (!inner) return [];
    return splitArrayItems(inner).map(parseTomlValue);
  }
  if (value === 'true') return true;
  if (value === 'false') return false;
  if (/^[+-]?\d+(?:\.\d+)?$/.test(value)) return Number(value);
  throw new Error(`unsupported TOML value: ${value}`);
}

function getOrCreateTable(root, tableName) {
  const parts = tableName.split('.');
  let target = root;
  for (const part of parts) {
    if (!part) throw new Error(`invalid table name: ${tableName}`);
    if (target[part] == null) {
      target[part] = {};
    } else if (Array.isArray(target[part])) {
      throw new Error(`table ${tableName} conflicts with an array table`);
    } else if (typeof target[part] !== 'object') {
      throw new Error(`table ${tableName} conflicts with a scalar value`);
    }
    target = target[part];
  }
  return target;
}

function getOrCreateArrayTable(root, tableName) {
  const parts = tableName.split('.');
  if (parts.length !== 1) {
    throw new Error(`nested array tables are not supported by this manifest parser: ${tableName}`);
  }
  const key = parts[0];
  if (root[key] == null) {
    root[key] = [];
  }
  if (!Array.isArray(root[key])) {
    throw new Error(`array table ${tableName} conflicts with an existing value`);
  }
  const entry = {};
  root[key].push(entry);
  return entry;
}

function parseToml(source) {
  const root = {};
  let current = root;
  const lines = source.split(/\r?\n/);
  for (let lineNumber = 1; lineNumber <= lines.length; lineNumber++) {
    const line = stripInlineComment(lines[lineNumber - 1]).trim();
    if (!line) continue;

    const arrayTable = line.match(/^\[\[\s*([A-Za-z0-9_.-]+)\s*\]\]$/);
    if (arrayTable) {
      current = getOrCreateArrayTable(root, arrayTable[1]);
      continue;
    }

    const table = line.match(/^\[\s*([A-Za-z0-9_.-]+)\s*\]$/);
    if (table) {
      current = getOrCreateTable(root, table[1]);
      continue;
    }

    const separator = findKeyValueSeparator(line);
    if (separator < 0) {
      throw new Error(`line ${lineNumber}: expected key = value`);
    }
    const key = line.slice(0, separator).trim();
    if (!/^[A-Za-z0-9_-]+$/.test(key)) {
      throw new Error(`line ${lineNumber}: unsupported key syntax: ${key}`);
    }
    current[key] = parseTomlValue(line.slice(separator + 1));
  }
  return root;
}

function requireString(scenario, key) {
  const value = scenario[key];
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`scenario ${scenario.id ?? '<unknown>'} is missing string field: ${key}`);
  }
  return value;
}

function requireStringArray(scenario, key) {
  const value = scenario[key];
  if (!Array.isArray(value) || value.some((entry) => typeof entry !== 'string' || entry.length === 0)) {
    throw new Error(`scenario ${scenario.id ?? '<unknown>'} is missing string array field: ${key}`);
  }
  return value;
}

function normalizeScenario(scenario) {
  const id = requireString(scenario, 'id');
  const status = scenario.status ?? 'runnable';
  if (!statusClasses.includes(status)) {
    throw new Error(`scenario ${id} has unsupported status: ${status}`);
  }
  return {
    id,
    title: requireString(scenario, 'title'),
    tier: requireString(scenario, 'tier'),
    transport: requireString(scenario, 'transport'),
    provider: requireString(scenario, 'provider'),
    terminalSize: requireString(scenario, 'terminal_size'),
    requiredHostTools: requireStringArray(scenario, 'required_host_tools'),
    requiredCapabilities: requireStringArray(scenario, 'required_capabilities'),
    validators: requireStringArray(scenario, 'validators'),
    artifacts: requireStringArray(scenario, 'artifacts'),
    manifestStatus: status,
    statusReasons: Array.isArray(scenario.status_reasons) ? scenario.status_reasons : [],
  };
}

function loadScenarioManifest(manifestPath) {
  const source = fs.readFileSync(manifestPath, 'utf8');
  const parsed = parseToml(source);
  if (!parsed.manifest || typeof parsed.manifest !== 'object') {
    throw new Error('manifest is missing [manifest] metadata');
  }
  if (!Array.isArray(parsed.scenarios)) {
    throw new Error('manifest is missing [[scenarios]] entries');
  }

  const scenarios = parsed.scenarios.map(normalizeScenario);
  const seen = new Set();
  for (const scenario of scenarios) {
    if (seen.has(scenario.id)) {
      throw new Error(`duplicate scenario id: ${scenario.id}`);
    }
    seen.add(scenario.id);
  }

  const missing = initialM19ScenarioIds.filter((id) => !seen.has(id));
  if (missing.length > 0) {
    throw new Error(`manifest is missing initial M19 scenario id(s): ${missing.join(', ')}`);
  }

  return {
    id: typeof parsed.manifest.id === 'string' ? parsed.manifest.id : 'unknown',
    title: typeof parsed.manifest.title === 'string' ? parsed.manifest.title : 'M19 UX scenarios',
    version: parsed.manifest.version ?? 'unknown',
    scenarios,
  };
}

function isExecutable(filePath) {
  try {
    fs.accessSync(filePath, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function executableName(name) {
  return process.platform === 'win32' ? `${name}.exe` : name;
}

function firstExecutable(candidates) {
  for (const candidate of candidates.filter(Boolean).map((entry) => path.resolve(entry))) {
    if (isExecutable(candidate)) return candidate;
  }
  return null;
}

function resolveSpecialHostTool(tool) {
  switch (tool) {
    case 'octos-bin':
      return firstExecutable([
        process.env.OCTOS_BIN,
        path.join(repoRoot, 'target', 'debug', executableName('octos')),
      ]);
    case 'octos-tui-bin':
      return firstExecutable([
        process.env.OCTOS_TUI_BIN,
        path.join(siblingOctosTuiRepo, 'target', 'debug', executableName('octos-tui')),
      ]);
    case 'octos-tui-onboarding-runner':
      return firstExecutable([
        process.env.OCTOS_TUI_ONBOARDING_RUNNER,
        process.env.OCTOS_M19_UX_TUI_RUNNER,
        path.join(siblingOctosTuiRepo, 'scripts', 'run-onboarding-tmux-soak.sh'),
      ]);
    case 'octos-tui-m15-runner':
      return firstExecutable([
        process.env.OCTOS_TUI_M15_RUNNER,
        path.join(siblingOctosTuiRepo, 'scripts', 'run-m15-live-tmux-ux-soak.sh'),
      ]);
    case 'octos-tui-m18-runner':
      return firstExecutable([
        process.env.OCTOS_TUI_M18_RUNNER,
        path.join(siblingOctosTuiRepo, 'scripts', 'run-m18-stdio-live-tmux-soak.sh'),
      ]);
    default:
      return undefined;
  }
}

function resolveHostTool(tool) {
  if (tool === 'node') {
    return process.execPath;
  }
  const specialTool = resolveSpecialHostTool(tool);
  if (specialTool !== undefined) {
    return specialTool;
  }
  if (tool.includes('/') || tool.includes('\\')) {
    return isExecutable(tool) ? tool : null;
  }
  const pathEntries = (process.env.PATH || '').split(path.delimiter).filter(Boolean);
  const extensions = process.platform === 'win32'
    ? (process.env.PATHEXT || '.EXE;.CMD;.BAT;.COM').split(';')
    : [''];
  for (const dir of pathEntries) {
    for (const ext of extensions) {
      const candidate = path.join(dir, `${tool}${ext}`);
      if (isExecutable(candidate)) return candidate;
    }
  }
  return null;
}

function classifyScenario(scenario, hostToolCache) {
  const toolResults = scenario.requiredHostTools.map((tool) => {
    if (!hostToolCache.has(tool)) {
      hostToolCache.set(tool, resolveHostTool(tool));
    }
    return { tool, path: hostToolCache.get(tool) };
  });
  const missingTools = toolResults.filter((entry) => !entry.path).map((entry) => entry.tool);

  if (scenario.manifestStatus === 'quarantined') {
    return {
      status: 'quarantined',
      reasons: scenario.statusReasons.length > 0
        ? scenario.statusReasons
        : ['scenario is marked quarantined in the manifest'],
    };
  }
  if (scenario.manifestStatus === 'skipped') {
    return {
      status: 'skipped',
      reasons: scenario.statusReasons.length > 0
        ? scenario.statusReasons
        : ['scenario is marked skipped in the manifest'],
    };
  }
  if (scenario.manifestStatus === 'blocked' || missingTools.length > 0) {
    const reasons = [...scenario.statusReasons];
    if (scenario.manifestStatus === 'blocked' && reasons.length === 0) {
      reasons.push('scenario is marked blocked in the manifest');
    }
    if (missingTools.length > 0) {
      reasons.push(`missing required host tool(s): ${missingTools.join(', ')}`);
    }
    if (scenario.requiredCapabilities.length > 0) {
      reasons.push('backend capabilities are declared in the manifest and are not probed by this list command');
    }
    return { status: 'blocked', reasons };
  }

  const reasons = [...scenario.statusReasons];
  reasons.push(scenario.requiredHostTools.length > 0
    ? `required host tools available: ${scenario.requiredHostTools.join(', ')}`
    : 'no host tools required by the manifest');
  if (scenario.requiredCapabilities.length > 0) {
    reasons.push('backend capabilities are declared in the manifest and are not probed by this list command');
  }
  return { status: 'runnable', reasons };
}

function buildListing(manifest, manifestPath) {
  const hostToolCache = new Map();
  const scenarios = manifest.scenarios.map((scenario) => {
    const classification = classifyScenario(scenario, hostToolCache);
    return {
      id: scenario.id,
      title: scenario.title,
      tier: scenario.tier,
      transport: scenario.transport,
      provider: scenario.provider,
      terminalSize: scenario.terminalSize,
      requiredHostTools: scenario.requiredHostTools,
      requiredCapabilities: scenario.requiredCapabilities,
      validators: scenario.validators,
      artifacts: scenario.artifacts,
      status: classification.status,
      reasons: classification.reasons,
    };
  });
  const summary = Object.fromEntries(statusClasses.map((status) => [status, 0]));
  for (const scenario of scenarios) {
    summary[scenario.status]++;
  }
  return {
    manifest: {
      id: manifest.id,
      title: manifest.title,
      version: manifest.version,
      path: path.relative(repoRoot, manifestPath),
    },
    statusClasses,
    scenarios,
    summary: {
      total: scenarios.length,
      ...summary,
    },
  };
}

function formatList(values) {
  return values.length > 0 ? values.join(', ') : '(none)';
}

function renderHuman(listing) {
  console.log(`${listing.manifest.title} (${listing.manifest.id}, version ${listing.manifest.version})`);
  console.log(`Manifest: ${listing.manifest.path}`);
  console.log(`Status classes: ${listing.statusClasses.join(', ')}`);
  console.log('');
  for (const scenario of listing.scenarios) {
    console.log(`- id: ${scenario.id}`);
    console.log(`  title: ${scenario.title}`);
    console.log(`  tier: ${scenario.tier}`);
    console.log(`  transport: ${scenario.transport}`);
    console.log(`  provider: ${scenario.provider}`);
    console.log(`  terminal size: ${scenario.terminalSize}`);
    console.log(`  required host tools: ${formatList(scenario.requiredHostTools)}`);
    console.log(`  required capabilities: ${formatList(scenario.requiredCapabilities)}`);
    console.log(`  validators: ${formatList(scenario.validators)}`);
    console.log(`  artifacts: ${formatList(scenario.artifacts)}`);
    console.log(`  status classification: ${scenario.status}`);
    console.log('  reasons:');
    for (const reason of scenario.reasons) {
      console.log(`    - ${reason}`);
    }
    console.log('');
  }
  console.log(
    `Summary: ${listing.summary.total} scenarios; ` +
    `runnable ${listing.summary.runnable}, ` +
    `skipped ${listing.summary.skipped}, ` +
    `blocked ${listing.summary.blocked}, ` +
    `quarantined ${listing.summary.quarantined}`,
  );
}

try {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    usage();
    process.exit(0);
  }
  const manifestPath = path.resolve(
    args.manifest || process.env.OCTOS_UX_SCENARIO_MANIFEST || defaultManifestPath,
  );
  const manifest = loadScenarioManifest(manifestPath);
  const listing = buildListing(manifest, manifestPath);
  if (args.json) {
    console.log(JSON.stringify(listing, null, 2));
  } else {
    renderHuman(listing);
  }
} catch (error) {
  console.error(`ux scenario list failed: ${error?.message ?? error}`);
  process.exit(1);
}
