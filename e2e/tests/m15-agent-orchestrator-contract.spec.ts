import { test, expect } from '@playwright/test';
import fs from 'node:fs';
import path from 'node:path';

const repoRoot = path.resolve(__dirname, '..', '..');
const workstreamPath = path.join(repoRoot, 'workstreams', 'M15-agent-goal-loop-autonomy.md');
const upcrPath = path.join(
  repoRoot,
  'docs',
  'OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_021_AGENT_GOAL_LOOP_AUTONOMY.md',
);

function readContractDocs(): string {
  return [
    fs.readFileSync(workstreamPath, 'utf8'),
    fs.readFileSync(upcrPath, 'utf8'),
  ].join('\n');
}

test.describe('M15 AgentOrchestrator contract docs', () => {
  test('pin the backend-owned orchestrator replacement contract', () => {
    const docs = readContractDocs();

    for (const required of [
      'AgentOrchestrator',
      'AppUI in-memory',
      'native subagent',
      'CLI-backed agent',
      'MCP-backed agent',
      'agent-orchestrator-ledger.jsonl',
      'transport-parity-report.json',
      'agent_control_forbidden',
      'coding.agent_control.v1',
    ]) {
      expect(docs, `missing contract term: ${required}`).toContain(required);
    }

    expect(docs).toMatch(/AppUI.*projection.*control surfaces/s);
    expect(docs).toMatch(/not from in-memory AppUI fixtures/);
    expect(docs).toMatch(/WebSocket and stdio.*identical shapes/s);
    expect(docs).toMatch(/Do not keep AppUI-only in-memory agent stubs/);
  });

  test('requires native, CLI, and MCP coverage in acceptance and soak evidence', () => {
    const workstream = fs.readFileSync(workstreamPath, 'utf8');
    const acceptanceStart = workstream.indexOf('### Acceptance Tests');
    const soakStart = workstream.indexOf('### Live Soak Evidence Plan');

    expect(acceptanceStart, 'missing acceptance section').toBeGreaterThan(0);
    expect(soakStart, 'missing soak evidence section').toBeGreaterThan(acceptanceStart);

    const acceptance = workstream.slice(acceptanceStart, soakStart);
    const soak = workstream.slice(soakStart);

    for (const required of ['native', 'CLI', 'MCP']) {
      expect(acceptance, `acceptance missing ${required}`).toContain(required);
      expect(soak, `soak plan missing ${required}`).toContain(required);
    }

    for (const artifact of [
      'native-agent-transcript.jsonl',
      'cli-agent-transcript.jsonl',
      'mcp-agent-transcript.jsonl',
      'agent-orchestrator-ledger.jsonl',
    ]) {
      expect(soak, `missing evidence artifact ${artifact}`).toContain(artifact);
    }
  });
});
