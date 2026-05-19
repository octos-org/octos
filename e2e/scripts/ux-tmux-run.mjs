#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '..', '..');
const defaultScenarioId = 'stdio-happy-path';
const requiredArtifacts = [
  'scenario.json',
  'summary.json',
  'validation.json',
  'launch-command.txt',
  'terminal-size.json',
  'input-replay.log',
  'server.log',
  'tui-capture.txt',
  'runtime-policy-stamp.json',
  'appui-transcript.jsonl',
];

const scenarios = new Map([
  [
    defaultScenarioId,
    {
      id: defaultScenarioId,
      title: 'Stdio Happy Path',
      transport: 'stdio',
      runner: 'onboarding-solo',
      finalMarker: 'M19_STDIO_HAPPY_PATH_FINAL_LINE',
      prompt:
        'Run the stdio happy path UX smoke. Open a session, send one short prompt, and finish with M19_STDIO_HAPPY_PATH_FINAL_LINE.',
    },
  ],
  [
    'websocket-happy-path',
    {
      id: 'websocket-happy-path',
      title: 'WebSocket Happy Path',
      transport: 'websocket',
      runner: 'onboarding-solo',
      finalMarker: 'M19_WEBSOCKET_HAPPY_PATH_FINAL_LINE',
      prompt:
        'Run the WebSocket happy path UX smoke. Open a session, send one short prompt, and finish with M19_WEBSOCKET_HAPPY_PATH_FINAL_LINE.',
    },
  ],
  [
    'tui-solo-onboarding',
    {
      id: 'tui-solo-onboarding',
      title: 'TUI Solo Onboarding',
      transport: 'stdio',
      runner: 'onboarding-solo',
      finalMarker: 'M19_TUI_SOLO_ONBOARDING_FINAL_LINE',
      prompt:
        'Run the solo onboarding UX smoke and finish with M19_TUI_SOLO_ONBOARDING_FINAL_LINE.',
    },
  ],
  [
    'narrow-layout',
    {
      id: 'narrow-layout',
      title: 'Narrow Terminal Layout',
      transport: 'stdio',
      runner: 'onboarding-solo',
      finalMarker: 'M19_NARROW_LAYOUT_FINAL_LINE',
      prompt:
        'Run the narrow-layout UX smoke and finish with M19_NARROW_LAYOUT_FINAL_LINE.',
    },
  ],
  [
    'provider-missing-recoverable',
    {
      id: 'provider-missing-recoverable',
      title: 'Missing Provider Is Recoverable',
      transport: 'stdio',
      runner: 'provider-missing',
      finalMarker: 'M19_PROVIDER_MISSING_FINAL_LINE',
      prompt: 'Show the missing provider recovery path.',
    },
  ],
  [
    'permission-selection',
    {
      id: 'permission-selection',
      title: 'Permission Selection',
      transport: 'stdio',
      runner: 'permission-selection',
      finalMarker: 'M19_PERMISSION_SELECTION_FINAL_LINE',
      prompt: 'Show the permission selection path.',
    },
  ],
  [
    'approval-denial',
    {
      id: 'approval-denial',
      title: 'Approval Denial',
      transport: 'stdio',
      runner: 'approval-denial',
      finalMarker: 'M19_APPROVAL_DENIAL_FINAL_LINE',
      prompt:
        'M9 approval fixture: request approval for printf m19-approval-denial and deny it in the TUI.',
    },
  ],
  [
    'task-subagent-tree',
    {
      id: 'task-subagent-tree',
      title: 'Task Subagent Tree',
      transport: 'stdio',
      runner: 'task-subagent-tree',
      finalMarker: 'M15CODEREVIEWFINALLINE',
      prompt:
        'Run M15 code review with live subagent orchestration through octos serve --stdio. Use supervised subagents and produce the final marker.',
    },
  ],
  [
    'restart-reconnect',
    {
      id: 'restart-reconnect',
      title: 'Restart And Reconnect',
      transport: 'websocket',
      runner: 'restart-reconnect',
      finalMarker: 'M19_RESTART_RECONNECT_FINAL_LINE',
      prompt: 'Show the restart and reconnect path.',
    },
  ],
  [
    'dropped-completion-backpressure',
    {
      id: 'dropped-completion-backpressure',
      title: 'Dropped Completion Backpressure',
      transport: 'websocket',
      runner: 'dropped-completion-backpressure',
      finalMarker: 'M19_DROPPED_COMPLETION_FINAL_LINE',
      prompt:
        'M9 replay-lossy fixture for M18 reconnect-style replay. This covers protocol/replay_lossy recovery, not a forced dropped turn/completed send failure.',
    },
  ],
  [
    'router-status-failover',
    {
      id: 'router-status-failover',
      title: 'Router Status Failover',
      transport: 'websocket',
      runner: null,
      blockedMessage:
        'Router status failover requires a fresh current-main port of the router/queue TUI UX.',
      finalMarker: 'M19_ROUTER_STATUS_FAILOVER_FINAL_LINE',
      prompt: 'Show the router status failover path.',
    },
  ],
]);

function usage() {
  return `Usage:
  node e2e/scripts/ux-tmux-run.mjs <scenario-id> [--dry-run]
  node e2e/scripts/ux-tmux-run.mjs --self-test [${defaultScenarioId}]

Options:
  --dry-run      Write the artifact skeleton without launching tmux.
  --self-test    Write and validate the artifact skeleton without launching tmux.
  --help         Show this help.

Scenarios:
  ${[...scenarios.keys()].join(', ')}

Environment:
  OCTOS_UX_TMUX_RUN_ID       Override run id.
  OCTOS_UX_TMUX_OUT_ROOT     Override output root. Default: e2e/test-results-ux.
  OCTOS_UX_TMUX_OUT_DIR      Override scenario output directory.
  OCTOS_UX_TMUX_TUI_RUNNER   Override octos-tui tmux runner script.
  OCTOS_TUI_REPO             Override octos-tui checkout. Default: ../octos-tui next to this repo.
`;
}

function parseArgs(argv) {
  let scenarioId = null;
  let dryRun = false;
  let selfTest = false;
  let help = false;

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dry-run') {
      dryRun = true;
    } else if (arg === '--self-test' || arg === 'self-test') {
      selfTest = true;
    } else if (arg === '--scenario') {
      i += 1;
      if (!argv[i]) throw new Error('--scenario requires a scenario id');
      scenarioId = argv[i];
    } else if (arg.startsWith('--scenario=')) {
      scenarioId = arg.slice('--scenario='.length);
    } else if (arg === 'run') {
      // Accept an explicit command word for callers that model this as ux:tmux:run.
    } else if (arg === '--help' || arg === '-h' || arg === 'help') {
      help = true;
    } else if (arg.startsWith('-')) {
      throw new Error(`unknown option: ${arg}`);
    } else if (!scenarioId) {
      scenarioId = arg;
    } else {
      throw new Error(`unexpected argument: ${arg}`);
    }
  }

  return {
    scenarioId: scenarioId || defaultScenarioId,
    dryRun,
    selfTest,
    help,
  };
}

function compactTimestamp(date = new Date()) {
  return date.toISOString().replace(/[-:]/g, '').replace(/\.\d{3}Z$/, 'Z');
}

function utcNow() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

function safeSlug(value) {
  return String(value).replace(/[^a-zA-Z0-9_.-]/g, '-');
}

function shellQuote(value) {
  const text = String(value);
  if (text.length === 0) return "''";
  return `'${text.replaceAll("'", "'\\''")}'`;
}

function positiveIntegerEnv(name, fallback) {
  const raw = process.env[name];
  if (!raw) return fallback;
  const value = Number(raw);
  if (!Number.isInteger(value) || value <= 0) {
    throw new Error(`${name} must be a positive integer, got ${raw}`);
  }
  return value;
}

function stablePortForRunId(runId) {
  let hash = 0;
  for (const char of runId) {
    hash = ((hash * 33) + char.charCodeAt(0)) >>> 0;
  }
  return 51000 + (hash % 10000);
}

function taskSubagentFixtureEnv(scenario, workdir) {
  if (scenario.runner !== 'task-subagent-tree') return {};
  return {
    OCTOS_M15_LIVE_SUBAGENT_FIXTURE: '1',
    OCTOS_TUI_M15_UX_OUTPUT_DIR: path.join(workdir, '.octos-m15-evidence'),
    OCTOS_TUI_M15_UX_WORKDIR: workdir,
    OCTOS_M15_LIVE_SUBAGENT_DELAY_SCALE:
      process.env.OCTOS_M15_LIVE_SUBAGENT_DELAY_SCALE || '0.25',
  };
}

function backendFixtureEnv(scenario, workdir) {
  return {
    OCTOS_M9_PROTOCOL_FIXTURES: '1',
    DEEPSEEK_API_KEY: 'dummy-key-for-ux-tmux',
    ...taskSubagentFixtureEnv(scenario, workdir),
  };
}

function shellEnvAssignments(env) {
  return Object.entries(env).map(([name, value]) => `${name}=${shellQuote(value)}`);
}

function isExecutable(file) {
  try {
    fs.accessSync(file, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function tmuxAvailable() {
  const result = spawnSync('tmux', ['-V'], { stdio: 'ignore' });
  return !result.error && result.status === 0;
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
}

function writeText(file, text) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, text, 'utf8');
}

function appendJsonl(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify(value)}\n`, 'utf8');
}

function artifactInfo(outDir, name) {
  const file = path.join(outDir, name);
  const exists = fs.existsSync(file);
  return {
    path: file,
    exists,
    bytes: exists ? fs.statSync(file).size : 0,
  };
}

function resolveContext({ scenarioId, selfTest }) {
  const scenario = scenarios.get(scenarioId);
  if (!scenario) {
    throw new Error(
      `unsupported scenario: ${scenarioId}. Supported scenarios: ${[...scenarios.keys()].join(', ')}`,
    );
  }

  const stamp = compactTimestamp();
  const runId =
    process.env.OCTOS_UX_TMUX_RUN_ID || `${selfTest ? 'ux-tmux-self-test' : 'ux-tmux'}-${stamp}`;
  const runKey = `${runId}-${scenario.id}`;
  const outRoot = path.resolve(
    process.env.OCTOS_UX_TMUX_OUT_ROOT || path.join(repoRoot, 'e2e', 'test-results-ux'),
  );
  const scenarioDir = path.resolve(
    process.env.OCTOS_UX_TMUX_OUT_DIR || path.join(outRoot, runId, scenario.id),
  );
  const runtimeRoot = path.resolve(
    process.env.OCTOS_UX_TMUX_RUNTIME_ROOT || path.join(os.tmpdir(), `octos-ux-tmux-${runKey}`),
  );
  const dataDir = path.resolve(process.env.OCTOS_UX_TMUX_DATA_DIR || path.join(runtimeRoot, 'data'));
  const workdir = path.resolve(
    process.env.OCTOS_UX_TMUX_WORKDIR || path.join(runtimeRoot, 'workspace'),
  );
  const replayFile = path.resolve(
    process.env.OCTOS_UX_TMUX_REPLAY || path.join(scenarioDir, 'input-replay.log'),
  );
  const tuiRepo = path.resolve(process.env.OCTOS_TUI_REPO || path.join(repoRoot, '..', 'octos-tui'));
  const lowerRunner = path.resolve(
    process.env.OCTOS_UX_TMUX_TUI_RUNNER
      || process.env.OCTOS_M19_UX_TUI_RUNNER
      || path.join(tuiRepo, 'scripts', 'run-onboarding-tmux-soak.sh'),
  );
  const octosBin = path.resolve(process.env.OCTOS_BIN || path.join(repoRoot, 'target', 'debug', 'octos'));
  const tuiBin = path.resolve(process.env.OCTOS_TUI_BIN || path.join(tuiRepo, 'target', 'debug', 'octos-tui'));
  const cols = positiveIntegerEnv('OCTOS_UX_TMUX_COLS', scenario.id === 'narrow-layout' ? 80 : 120);
  const rows = positiveIntegerEnv('OCTOS_UX_TMUX_ROWS', scenario.id === 'narrow-layout' ? 24 : 40);
  const port = positiveIntegerEnv('OCTOS_UX_TMUX_PORT', stablePortForRunId(runKey));
  const profileId = process.env.OCTOS_UX_TMUX_PROFILE || 'coding';
  const sessionId =
    process.env.OCTOS_UX_TMUX_SESSION_ID || `${profileId}:local:ux:${scenario.id}:${runId}`;
  const sessionName =
    process.env.OCTOS_UX_TMUX_SESSION || `octos-ux-${safeSlug(runId)}-${safeSlug(scenario.id)}`;
  const fixtureEnv = taskSubagentFixtureEnv(scenario, workdir);
  const backendEnv = backendFixtureEnv(scenario, workdir);
  const backendCommand =
    process.env.OCTOS_UX_TMUX_BACKEND_COMMAND
    || [
      'env',
      ...shellEnvAssignments(backendEnv),
      shellQuote(octosBin),
      'serve',
      '--stdio',
      '--data-dir',
      shellQuote(dataDir),
      '--cwd',
      shellQuote(workdir),
    ].join(' ');
  const websocketEndpoint = `ws://127.0.0.1:${port}/api/ui-protocol/ws`;
  const authToken = process.env.OCTOS_UX_TMUX_AUTH_TOKEN || 'octos-tui-onboarding-soak-token';
  const launchCommand =
    process.env.OCTOS_UX_TMUX_LAUNCH_COMMAND
    || (scenario.transport === 'websocket'
      ? `${shellQuote(tuiBin)} --mode protocol --endpoint ${shellQuote(websocketEndpoint)} --auth-token ${shellQuote(authToken)}`
      : `${shellQuote(tuiBin)} --mode protocol --stdio-command ${shellQuote(backendCommand)}`);

  return {
    scenario,
    runId,
    runKey,
    outRoot,
    scenarioDir,
    runtimeRoot,
    dataDir,
    workdir,
    replayFile,
    tuiRepo,
    lowerRunner,
    octosBin,
    tuiBin,
    cols,
    rows,
    port,
    websocketEndpoint,
    authToken,
    profileId,
    sessionId,
    sessionName,
    fixtureEnv,
    backendEnv,
    backendCommand,
    launchCommand,
  };
}

function writeWorkspaceFixture(ctx) {
  fs.mkdirSync(ctx.workdir, { recursive: true });
  writeText(
    path.join(ctx.workdir, 'README.md'),
    `# ${ctx.scenario.title}\n\nMinimal workspace fixture for ${ctx.scenario.id}.\n`,
  );
}

function writeReplay(ctx) {
  writeText(
    ctx.replayFile,
    [
      `# ${ctx.scenario.title} UX tmux replay.`,
      'sleep 3',
      'capture tui-capture-start.txt',
      `line ${ctx.scenario.prompt}`,
      'sleep 4',
      'capture tui-capture.txt',
      'exit',
      'sleep 1',
      'capture tui-exit-capture.txt',
      '',
    ].join('\n'),
  );
}

function writeRuntimePolicyStamp(ctx, generatedAt, { force = false } = {}) {
  const file = path.join(ctx.scenarioDir, 'runtime-policy-stamp.json');
  if (!force && fs.existsSync(file)) return;
  writeJson(file, {
    schema: 'octos.ux.runtime_policy_stamp.v1',
    generated_at: generatedAt,
    source: 'harness',
    run_id: ctx.runId,
    scenario_id: ctx.scenario.id,
    transport: ctx.scenario.transport,
    backend: ctx.scenario.transport === 'websocket' ? 'octos serve --websocket' : 'octos serve --stdio',
    stamp: {
      profile_id: ctx.profileId,
      session_id: ctx.sessionId,
      approval_policy: process.env.OCTOS_APPROVAL_POLICY || null,
      sandbox_mode: process.env.OCTOS_SANDBOX || 'inherits harness environment',
      network_access: process.env.OCTOS_NETWORK || 'inherits harness environment',
    },
    terminal: {
      cols: ctx.cols,
      rows: ctx.rows,
    },
    fixture_env: ctx.fixtureEnv,
  });
}

function collectArtifactNames(outDir) {
  const names = new Set(requiredArtifacts);
  const visit = (dir) => {
    if (!fs.existsSync(dir)) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const file = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        visit(file);
      } else if (entry.isFile()) {
        names.add(path.relative(outDir, file));
      }
    }
  };
  visit(outDir);
  return [...names].sort((left, right) => {
    const leftRequired = requiredArtifacts.indexOf(left);
    const rightRequired = requiredArtifacts.indexOf(right);
    if (leftRequired !== -1 || rightRequired !== -1) {
      if (leftRequired === -1) return 1;
      if (rightRequired === -1) return -1;
      return leftRequired - rightRequired;
    }
    return left.localeCompare(right);
  });
}

function writeArtifactSkeleton(ctx, options) {
  const generatedAt = utcNow();
  const placeholder = options.placeholderArtifacts !== false;
  fs.mkdirSync(ctx.scenarioDir, { recursive: true });
  fs.mkdirSync(ctx.dataDir, { recursive: true });
  fs.mkdirSync(ctx.runtimeRoot, { recursive: true });
  writeWorkspaceFixture(ctx);
  writeReplay(ctx);

  writeJson(path.join(ctx.scenarioDir, 'scenario.json'), {
    schema: 'octos.ux.scenario.v1',
    artifact_abi: 'octos.ux.artifacts.v1',
    generated_at: generatedAt,
    id: ctx.scenario.id,
    scenario_id: ctx.scenario.id,
    title: ctx.scenario.title,
    transport: ctx.scenario.transport,
    run_id: ctx.runId,
    output_dir: ctx.scenarioDir,
    runtime_root: ctx.runtimeRoot,
    data_dir: ctx.dataDir,
    workspace: ctx.workdir,
    replay_file: ctx.replayFile,
    session_id: ctx.sessionId,
    profile_id: ctx.profileId,
    final_marker: ctx.scenario.finalMarker,
    lower_runner: ctx.lowerRunner,
    fixture_env: ctx.fixtureEnv,
    required_artifacts: requiredArtifacts,
  });

  writeText(path.join(ctx.scenarioDir, 'launch-command.txt'), `${ctx.launchCommand}\n`);
  writeJson(path.join(ctx.scenarioDir, 'terminal-size.json'), {
    schema: 'octos.ux.terminal_size.v1',
    generated_at: generatedAt,
    cols: ctx.cols,
    rows: ctx.rows,
  });
  writeRuntimePolicyStamp(ctx, generatedAt, { force: placeholder });

  if (placeholder) {
    writeText(
      path.join(ctx.scenarioDir, 'server.log'),
      [
        `[${generatedAt}] placeholder server log`,
        `status=${options.status}`,
        `message=${options.message}`,
        `backend_command=${ctx.backendCommand}`,
        '',
      ].join('\n'),
    );
    writeText(
      path.join(ctx.scenarioDir, 'tui-capture.txt'),
      [
        `[${ctx.scenario.title}] placeholder TUI capture`,
        `status: ${options.status}`,
        `message: ${options.message}`,
        `tmux launched: ${options.realTmuxLaunched ? 'yes' : 'no'}`,
        '',
      ].join('\n'),
    );
    writeText(path.join(ctx.scenarioDir, 'appui-transcript.jsonl'), '');
    appendJsonl(path.join(ctx.scenarioDir, 'appui-transcript.jsonl'), {
      direction: 'client_to_server',
      frame: {
        jsonrpc: '2.0',
        id: 'placeholder-1',
        method: 'harness/placeholder',
        params: {
          generated_at: generatedAt,
          scenario_id: ctx.scenario.id,
          status: options.status,
          message: options.message,
          real_tmux_launched: Boolean(options.realTmuxLaunched),
        },
      },
    });
    appendJsonl(path.join(ctx.scenarioDir, 'appui-transcript.jsonl'), {
      direction: 'server_to_client',
      frame: {
        jsonrpc: '2.0',
        id: 'placeholder-1',
        result: {
          status: options.status,
          message: options.message,
        },
      },
    });
  } else {
    for (const name of ['server.log', 'tui-capture.txt', 'appui-transcript.jsonl']) {
      const file = path.join(ctx.scenarioDir, name);
      if (!fs.existsSync(file)) writeText(file, '');
    }
  }

  const summaryPath = path.join(ctx.scenarioDir, 'summary.json');
  const summary = {
    schema: 'octos.ux.summary.v1',
    generated_at: generatedAt,
    ok: Boolean(options.ok),
    status: options.status,
    mode: options.mode,
    message: options.message,
    blockers: options.blockers || [],
    scenario_id: ctx.scenario.id,
    run_id: ctx.runId,
    output_dir: ctx.scenarioDir,
    placeholder_artifacts: placeholder,
    real_tmux_launched: Boolean(options.realTmuxLaunched),
    lower_runner: {
      path: ctx.lowerRunner,
      exists: fs.existsSync(ctx.lowerRunner),
      executable: isExecutable(ctx.lowerRunner),
    },
    artifacts: {},
  };
  writeJson(summaryPath, summary);
  summary.artifacts = Object.fromEntries(
    collectArtifactNames(ctx.scenarioDir).map((name) => [name, artifactInfo(ctx.scenarioDir, name)]),
  );
  writeJson(summaryPath, summary);
}

function validateArtifactSkeleton(ctx) {
  const missing = requiredArtifacts.filter((name) => {
    const file = path.join(ctx.scenarioDir, name);
    return !fs.existsSync(file) || fs.statSync(file).size === 0;
  });
  if (missing.length > 0) {
    throw new Error(`self-test missing or empty artifacts: ${missing.join(', ')}`);
  }
}

function refreshSummaryArtifacts(ctx, validationStatus = null) {
  const summaryPath = path.join(ctx.scenarioDir, 'summary.json');
  if (!fs.existsSync(summaryPath)) return;
  const summary = JSON.parse(fs.readFileSync(summaryPath, 'utf8'));
  if (validationStatus !== null) {
    summary.validation_status = validationStatus === 0 ? 'passed' : 'failed';
    if (validationStatus !== 0) {
      summary.ok = false;
      summary.status = 'failed';
      summary.message = `${summary.message}; validation failed; see validation.json`;
    }
  }
  summary.artifacts = Object.fromEntries(
    collectArtifactNames(ctx.scenarioDir).map((name) => [name, artifactInfo(ctx.scenarioDir, name)]),
  );
  writeJson(summaryPath, summary);
}

function missingRunnerBlocker(ctx) {
  if (!fs.existsSync(ctx.lowerRunner)) {
    return `octos-tui tmux runner is missing: ${ctx.lowerRunner}`;
  }
  if (!isExecutable(ctx.lowerRunner)) {
    return `octos-tui tmux runner is not executable: ${ctx.lowerRunner}`;
  }
  return null;
}

function tmuxHasSession(sessionName) {
  const result = spawnSync('tmux', ['has-session', '-t', sessionName], { stdio: 'ignore' });
  return !result.error && result.status === 0;
}

function launchWebsocketTuiFallback(ctx, env) {
  const outputLog = path.join(ctx.scenarioDir, 'tui-process.log');
  const apiKeyEnv = process.env.OCTOS_TUI_SOAK_EXPECT_API_KEY_ENV || 'AUTODL_API_KEY';
  const apiKey = env.OCTOS_TUI_SOAK_API_KEY || 'octos-m19-placeholder-key';
  const command = [
    'cd',
    shellQuote(ctx.workdir),
    '&&',
    'env',
    shellQuote(`${apiKeyEnv}=${apiKey}`),
    shellQuote(ctx.tuiBin),
    '--mode',
    'protocol',
    '--endpoint',
    shellQuote(ctx.websocketEndpoint),
    '--auth-token',
    shellQuote(ctx.authToken),
    '--session',
    shellQuote(ctx.sessionId),
    '--profile-id',
    shellQuote(ctx.profileId),
    '--cwd',
    shellQuote(ctx.workdir),
    '--theme',
    shellQuote(process.env.OCTOS_TUI_SOAK_THEME || 'codex'),
    ';',
    'exit_code=$?;',
    'echo',
    shellQuote('octos-tui exited with status'),
    '"$exit_code"',
    ';',
    'echo',
    shellQuote('octos-tui exited with status'),
    '"$exit_code"',
    '>>',
    shellQuote(outputLog),
    ';',
    'sleep',
    shellQuote(process.env.OCTOS_TUI_SOAK_EXIT_HOLD_SECS || '30'),
  ].join(' ');
  writeText(path.join(ctx.scenarioDir, 'tui-fallback-command.txt'), `${command}\n`);
  const result = spawnSync('tmux', ['new-session', '-d', '-s', ctx.sessionName, command], {
    cwd: repoRoot,
    env,
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  return result.status || 0;
}

function resizeTmuxWindow(ctx) {
  const resize = spawnSync('tmux', [
    'resize-window',
    '-t',
    ctx.sessionName,
    '-x',
    String(ctx.cols),
    '-y',
    String(ctx.rows),
  ], { stdio: 'ignore' });
  if (resize.error) throw resize.error;
  if (resize.status !== 0) {
    console.error(`tmux resize-window failed for ${ctx.sessionName}; capture validation will report if the TUI pane exited`);
  }
}

function captureTmuxPane(sessionName, outputFile) {
  const result = spawnSync('tmux', [
    'capture-pane',
    '-t',
    sessionName,
    '-p',
    '-J',
    '-S',
    '-300',
  ], { encoding: 'utf8' });
  if (result.error) throw result.error;
  writeText(
    outputFile,
    result.status === 0
      ? result.stdout
      : `tmux capture-pane failed for ${sessionName}: ${result.stderr || `status ${result.status}`}\n`,
  );
  return result.status === 0 ? 0 : (result.status || 1);
}

function runRestartReconnectProbe(ctx, phase, env) {
  const result = spawnSync(process.execPath, [
    path.join(scriptDir, 'm19-restart-reconnect-probe.mjs'),
  ], {
    cwd: repoRoot,
    env: {
      ...env,
      OCTOS_M19_RESTART_PHASE: phase,
      OCTOS_M19_RESTART_ARTIFACT_DIR: ctx.scenarioDir,
      OCTOS_M19_RESTART_WS_ENDPOINT: ctx.websocketEndpoint,
      OCTOS_M19_RESTART_AUTH_TOKEN: ctx.authToken,
      OCTOS_M19_RESTART_PROFILE_ID: ctx.profileId,
      OCTOS_M19_RESTART_SESSION_ID: ctx.sessionId,
      OCTOS_M19_RESTART_WORKSPACE: ctx.workdir,
    },
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  return result.status || 0;
}

function runBackpressureReplayProbe(ctx, env) {
  const result = spawnSync(process.execPath, [
    path.join(scriptDir, 'm19-backpressure-replay-probe.mjs'),
  ], {
    cwd: repoRoot,
    env: {
      ...env,
      OCTOS_M19_BACKPRESSURE_ARTIFACT_DIR: ctx.scenarioDir,
      OCTOS_M19_BACKPRESSURE_WS_ENDPOINT: ctx.websocketEndpoint,
      OCTOS_M19_BACKPRESSURE_AUTH_TOKEN: ctx.authToken,
      OCTOS_M19_BACKPRESSURE_PROFILE_ID: ctx.profileId,
      OCTOS_M19_BACKPRESSURE_SESSION_ID: ctx.sessionId,
      OCTOS_M19_BACKPRESSURE_WORKSPACE: ctx.workdir,
      OCTOS_M19_BACKPRESSURE_PROMPT: ctx.scenario.prompt,
    },
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  return result.status || 0;
}

function runRestartReconnectScenario(ctx, action, env) {
  let status = 0;
  const start = action('start');
  if (start.error) throw start.error;
  if (start.status !== 0) return start.status || 1;
  if (!tmuxHasSession(ctx.sessionName)) {
    console.error(`TUI session ${ctx.sessionName} was missing after lower runner start; relaunching real websocket TUI`);
    status = launchWebsocketTuiFallback(ctx, env);
    if (status !== 0) return status;
  }
  resizeTmuxWindow(ctx);

  const preTurn = action('send-turn', true, {
    OCTOS_TUI_SOAK_PROMPT:
      'Before backend restart, answer briefly so the reconnect fixture has visible session state.',
    OCTOS_TUI_SOAK_TURN_WAIT_SECS: process.env.OCTOS_TUI_SOAK_RESTART_PRE_TURN_WAIT_SECS || '10',
  });
  if (preTurn.error) throw preTurn.error;
  if (preTurn.status !== 0) status = preTurn.status || 1;
  captureTmuxPane(ctx.sessionName, path.join(ctx.scenarioDir, 'tui-capture-pre-restart.txt'));
  if (status === 0) status = runRestartReconnectProbe(ctx, 'pre', env);

  if (status === 0) {
    const restart = action('restart-server', true, {
      OCTOS_TUI_SOAK_SERVER_WAIT_SECS:
        process.env.OCTOS_TUI_SOAK_RESTART_SERVER_WAIT_SECS || '20',
    });
    if (restart.error) throw restart.error;
    if (restart.status !== 0) status = restart.status || 1;
  }

  if (status === 0) {
    const postTurn = action('send-turn', true, {
      OCTOS_TUI_SOAK_PROMPT:
        `After backend restart, confirm the TUI reconnected and finish with ${ctx.scenario.finalMarker}.`,
      OCTOS_TUI_SOAK_TURN_WAIT_SECS:
        process.env.OCTOS_TUI_SOAK_RESTART_POST_TURN_WAIT_SECS || '20',
    });
    if (postTurn.error) throw postTurn.error;
    if (postTurn.status !== 0) status = postTurn.status || 1;
  }
  captureTmuxPane(ctx.sessionName, path.join(ctx.scenarioDir, 'tui-capture-post-reconnect.txt'));
  if (status === 0) status = runRestartReconnectProbe(ctx, 'post', env);
  return status;
}

function runDroppedCompletionBackpressureScenario(ctx, action, env) {
  let status = 0;
  const start = action('start');
  if (start.error) throw start.error;
  if (start.status !== 0) return start.status || 1;
  if (!tmuxHasSession(ctx.sessionName)) {
    console.error(`TUI session ${ctx.sessionName} was missing after lower runner start; relaunching real websocket TUI`);
    status = launchWebsocketTuiFallback(ctx, env);
    if (status !== 0) return status;
  }
  resizeTmuxWindow(ctx);

  const drive = action('drive-dropped-completion-backpressure', true, {
    OCTOS_TUI_SOAK_BACKPRESSURE_PROMPT: ctx.scenario.prompt,
  });
  if (drive.error) throw drive.error;
  if (drive.status !== 0) status = drive.status || 1;
  captureTmuxPane(ctx.sessionName, path.join(ctx.scenarioDir, 'tui-capture-backpressure-final.txt'));
  if (status === 0) status = runBackpressureReplayProbe(ctx, env);
  return status;
}

function runnerSteps(ctx) {
  if (ctx.scenario.runner === 'restart-reconnect' || ctx.scenario.runner === 'dropped-completion-backpressure') {
    return [];
  }
  if (ctx.scenario.runner === 'provider-missing') {
    return ['start', 'drive-provider-missing', 'drive-solo'];
  }
  if (ctx.scenario.runner === 'permission-selection') {
    return ['start', 'drive-permissions', 'drive-solo'];
  }
  if (ctx.scenario.runner === 'approval-denial') {
    return ['start', 'drive-approval-denial', 'drive-solo'];
  }
  if (ctx.scenario.runner === 'task-subagent-tree') {
    return ['start', 'drive-task-subagent-tree', 'drive-solo'];
  }
  return ['start', 'drive-solo'];
}

function runLowerRunner(ctx) {
  const shouldInitProfileLlm = ctx.scenario.runner === 'provider-missing' ? '0' : '1';
  const env = {
    ...process.env,
    OCTOS_REPO: repoRoot,
    OCTOS_BIN: ctx.octosBin,
    OCTOS_TUI_BIN: ctx.tuiBin,
    OCTOS_TUI_SOAK_RUN_ID: ctx.runId,
    OCTOS_TUI_SOAK_ARTIFACT_DIR: ctx.scenarioDir,
    OCTOS_TUI_SOAK_RUNTIME_ROOT: ctx.runtimeRoot,
    OCTOS_TUI_SOAK_WORKSPACE: ctx.workdir,
    OCTOS_TUI_SOAK_DATA_DIR: ctx.dataDir,
    OCTOS_TUI_SOAK_TRANSPORT: ctx.scenario.transport === 'websocket' ? 'ws' : ctx.scenario.transport,
    OCTOS_TUI_SOAK_PROFILE: ctx.profileId,
    OCTOS_TUI_SOAK_SESSION: ctx.sessionId,
    OCTOS_TUI_SOAK_SERVER_SESSION: `octos-onboard-server-${safeSlug(ctx.runKey)}`,
    OCTOS_TUI_SOAK_TUI_SESSION: ctx.sessionName,
    OCTOS_TUI_SOAK_LOCAL_NAME: process.env.OCTOS_TUI_SOAK_LOCAL_NAME || ctx.profileId,
    OCTOS_TUI_SOAK_LOCAL_USERNAME: process.env.OCTOS_TUI_SOAK_LOCAL_USERNAME || ctx.profileId,
    OCTOS_TUI_SOAK_LOCAL_EMAIL: process.env.OCTOS_TUI_SOAK_LOCAL_EMAIL || `${ctx.profileId}@example.invalid`,
    OCTOS_TUI_SOAK_API_KEY: process.env.OCTOS_TUI_SOAK_API_KEY || 'octos-m19-placeholder-key',
    OCTOS_TUI_SOAK_INIT_PROFILE_LLM:
      process.env.OCTOS_TUI_SOAK_INIT_PROFILE_LLM || shouldInitProfileLlm,
    OCTOS_TUI_SOAK_PORT: String(ctx.port),
    OCTOS_TUI_SOAK_AUTH_TOKEN: ctx.authToken,
    OCTOS_TUI_SOAK_OPEN_SESSION: 'auto',
    OCTOS_TUI_SOAK_REQUIRE_PROFILE: '0',
    OCTOS_TUI_SOAK_SOLO_STRICT: process.env.OCTOS_TUI_SOAK_SOLO_STRICT || '0',
    OCTOS_TUI_SOAK_SERVER_WAIT_SECS:
      process.env.OCTOS_TUI_SOAK_SERVER_WAIT_SECS
      || (ctx.scenario.transport === 'websocket' ? '4' : '1'),
    OCTOS_TUI_SOAK_TUI_WAIT_SECS: process.env.OCTOS_TUI_SOAK_TUI_WAIT_SECS || '2',
    OCTOS_TUI_SOAK_EXIT_HOLD_SECS: process.env.OCTOS_TUI_SOAK_EXIT_HOLD_SECS || '30',
    OCTOS_M9_PROTOCOL_FIXTURES: '1',
    ...ctx.fixtureEnv,
  };

  const action = (name, inherit = true, envOverrides = {}) => spawnSync(ctx.lowerRunner, [name], {
    cwd: repoRoot,
    env: { ...env, ...envOverrides },
    stdio: inherit ? 'inherit' : 'pipe',
  });

  let status = 0;
  if (ctx.scenario.runner === 'restart-reconnect') {
    status = runRestartReconnectScenario(ctx, action, env);
  } else if (ctx.scenario.runner === 'dropped-completion-backpressure') {
    status = runDroppedCompletionBackpressureScenario(ctx, action, env);
  } else {
    for (const step of runnerSteps(ctx)) {
      if (status !== 0) break;
      const stepEnv = ctx.scenario.runner === 'provider-missing' && step === 'drive-solo'
        ? { OCTOS_TUI_SOAK_INIT_PROFILE_LLM: '1' }
        : {};
      const result = action(step, true, stepEnv);
      if (result.error) throw result.error;
      if (result.status !== 0) status = result.status || 1;
      if (step === 'start' && status === 0) {
        if (ctx.scenario.transport === 'websocket' && !tmuxHasSession(ctx.sessionName)) {
          console.error(`TUI session ${ctx.sessionName} was missing after lower runner start; relaunching real websocket TUI`);
          const fallbackStatus = launchWebsocketTuiFallback(ctx, env);
          if (fallbackStatus !== 0) status = fallbackStatus;
        }
        resizeTmuxWindow(ctx);
      }
    }
  }

  const capture = action('capture');
  if (capture.error && status === 0) throw capture.error;
  if (capture.status !== 0 && status === 0) status = capture.status || 1;

  if (process.env.OCTOS_UX_TMUX_KEEP_SESSION !== '1') {
    action('stop', false);
  }

  return status;
}

function runValidation(ctx) {
  const result = spawnSync(process.execPath, [path.join(scriptDir, 'ux-tmux-validate.mjs'), ctx.scenarioDir], {
    cwd: repoRoot,
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  const status = result.status === 0 ? 0 : (result.status ?? 1);
  refreshSummaryArtifacts(ctx, status);
  return status;
}

function realMode(ctx) {
  if (!ctx.scenario.runner) {
    const blocker = ctx.scenario.blockedMessage || `scenario ${ctx.scenario.id} has no real tmux runner yet`;
    writeArtifactSkeleton(ctx, {
      mode: 'run',
      status: 'blocked',
      ok: false,
      message: blocker,
      blockers: [blocker],
      placeholderArtifacts: true,
      realTmuxLaunched: false,
    });
    const validationStatus = runValidation(ctx);
    console.error(`${blocker}\nArtifacts: ${ctx.scenarioDir}`);
    return validationStatus === 0 ? 2 : validationStatus;
  }

  const runnerBlocker = missingRunnerBlocker(ctx);
  if (runnerBlocker) {
    writeArtifactSkeleton(ctx, {
      mode: 'run',
      status: 'blocked',
      ok: false,
      message: runnerBlocker,
      blockers: [runnerBlocker],
      placeholderArtifacts: true,
      realTmuxLaunched: false,
    });
    const validationStatus = runValidation(ctx);
    console.error(`${runnerBlocker}\nArtifacts: ${ctx.scenarioDir}`);
    return validationStatus === 0 ? 2 : validationStatus;
  }

  if (!tmuxAvailable()) {
    const blocker = 'tmux is required for real ux tmux mode';
    writeArtifactSkeleton(ctx, {
      mode: 'run',
      status: 'blocked',
      ok: false,
      message: blocker,
      blockers: [blocker],
      placeholderArtifacts: true,
      realTmuxLaunched: false,
    });
    const validationStatus = runValidation(ctx);
    console.error(`${blocker}\nArtifacts: ${ctx.scenarioDir}`);
    return validationStatus === 0 ? 2 : validationStatus;
  }

  writeArtifactSkeleton(ctx, {
    mode: 'run',
    status: 'starting',
    ok: false,
    message: 'real tmux run starting',
    placeholderArtifacts: false,
    realTmuxLaunched: false,
  });

  let status = 1;
  try {
    status = runLowerRunner(ctx);
  } finally {
    writeArtifactSkeleton(ctx, {
      mode: 'run',
      status: status === 0 ? 'passed' : 'failed',
      ok: status === 0,
      message: status === 0 ? 'real tmux run completed' : `lower runner exited with status ${status}`,
      placeholderArtifacts: false,
      realTmuxLaunched: true,
    });
  }
  const validationStatus = runValidation(ctx);
  console.log(`UX tmux artifacts: ${ctx.scenarioDir}`);
  return status === 0 ? validationStatus : status;
}

function dryRun(ctx) {
  writeArtifactSkeleton(ctx, {
    mode: 'dry-run',
    status: 'blocked',
    ok: true,
    message: 'dry run wrote artifact skeleton; tmux was not launched',
    placeholderArtifacts: true,
    realTmuxLaunched: false,
  });
  const validationStatus = runValidation(ctx);
  console.log(`Dry run wrote UX tmux artifact skeleton: ${ctx.scenarioDir}`);
  return validationStatus;
}

function selfTest(ctx) {
  writeArtifactSkeleton(ctx, {
    mode: 'self-test',
    status: 'passed',
    ok: true,
    message: 'self-test wrote and validated artifact skeleton; tmux was not launched',
    placeholderArtifacts: true,
    realTmuxLaunched: false,
  });
  const validationStatus = runValidation(ctx);
  if (validationStatus !== 0) {
    throw new Error(`self-test validator failed with status ${validationStatus}`);
  }
  validateArtifactSkeleton(ctx);
  console.log(`Self-test passed: ${ctx.scenarioDir}`);
  return 0;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(usage());
    return 0;
  }

  const ctx = resolveContext(args);
  if (args.selfTest) return selfTest(ctx);
  if (args.dryRun) return dryRun(ctx);
  return realMode(ctx);
}

try {
  process.exitCode = main();
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
