import { expect, test } from '@playwright/test';
import fs from 'node:fs';
import path from 'node:path';

const ITERATIONS = 50;
const REGRESSION_DIR = path.resolve(
  __dirname,
  '../../test-fixtures/regressions',
);
const REGRESSION_FIXTURE_SCHEMA = 'octos.thread-binding-regression.v1';

type Role = 'user' | 'assistant' | 'tool';

interface TranscriptRecord {
  role: Role;
  content: string;
  at_ms: number;
  thread_id: string;
  client_message_id?: string;
  response_to_client_message_id?: string;
}

interface TurnCase {
  cmid: string;
  prompt: string;
  send_at_ms: number;
}

interface CompletionCase {
  turn_index: number;
  role: Exclude<Role, 'user'>;
  complete_at_ms: number;
  content: string;
}

interface ScenarioCase {
  schema: 'octos.thread-binding.property.v1';
  seed: number;
  turns: TurnCase[];
  completions: CompletionCase[];
}

interface BindingViolation {
  record_index: number;
  role: Role;
  expected_thread_id: string;
  actual_thread_id: string;
  response_to_client_message_id?: string;
  content_preview: string;
}

interface RegressionFixture {
  schema: string;
  name: string;
  records: TranscriptRecord[];
  expected_violations: Array<Partial<BindingViolation>>;
}

function mulberry32(seed: number) {
  return () => {
    let t = (seed += 0x6d2b79f5);
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function intBetween(rand: () => number, min: number, max: number) {
  return Math.floor(rand() * (max - min + 1)) + min;
}

function roleRank(role: Role) {
  return role === 'user' ? 0 : 1;
}

function generateScenario(seed: number): ScenarioCase {
  const rand = mulberry32(seed);
  const turnCount = intBetween(rand, 3, 8);
  const turns: TurnCase[] = [];
  let sendAtMs = 0;

  for (let i = 0; i < turnCount; i++) {
    sendAtMs += i === 0 ? 0 : intBetween(rand, 0, 2500);
    turns.push({
      cmid: `cmid-${seed}-${i + 1}`,
      prompt: `property prompt ${seed}.${i + 1}`,
      send_at_ms: sendAtMs,
    });
  }

  const lastSendAtMs = turns[turns.length - 1].send_at_ms;
  const completions: CompletionCase[] = [];
  for (const [i, turn] of turns.entries()) {
    let completeAtMs = turn.send_at_ms + intBetween(rand, 0, 35_000);
    if (i === 0) {
      completeAtMs = Math.max(completeAtMs, lastSendAtMs + 1);
    }
    completions.push({
      turn_index: i,
      role: 'assistant',
      complete_at_ms: completeAtMs,
      content: `assistant result for ${turn.cmid}`,
    });

    if (rand() < 0.55) {
      completions.push({
        turn_index: i,
        role: 'tool',
        complete_at_ms: turn.send_at_ms + intBetween(rand, 0, 35_000),
        content: `tool result for ${turn.cmid}`,
      });
    }
  }

  completions.sort(
    (a, b) =>
      a.complete_at_ms - b.complete_at_ms ||
      a.turn_index - b.turn_index ||
      a.role.localeCompare(b.role),
  );

  return {
    schema: 'octos.thread-binding.property.v1',
    seed,
    turns,
    completions,
  };
}

function latestTurnAt(scenario: ScenarioCase, atMs: number) {
  let latest = scenario.turns[0];
  for (const turn of scenario.turns) {
    if (turn.send_at_ms <= atMs) latest = turn;
  }
  return latest;
}

function materializeScenario(
  scenario: ScenarioCase,
  mode: 'bound-at-spawn' | 'sticky-latest-user',
): TranscriptRecord[] {
  const records: TranscriptRecord[] = [];

  for (const turn of scenario.turns) {
    records.push({
      role: 'user',
      content: turn.prompt,
      at_ms: turn.send_at_ms,
      client_message_id: turn.cmid,
      thread_id: turn.cmid,
    });
  }

  for (const completion of scenario.completions) {
    const origin = scenario.turns[completion.turn_index];
    const sticky = latestTurnAt(scenario, completion.complete_at_ms);
    records.push({
      role: completion.role,
      content: completion.content,
      at_ms: completion.complete_at_ms,
      response_to_client_message_id: origin.cmid,
      thread_id: mode === 'bound-at-spawn' ? origin.cmid : sticky.cmid,
    });
  }

  records.sort(
    (a, b) =>
      a.at_ms - b.at_ms ||
      roleRank(a.role) - roleRank(b.role) ||
      a.content.localeCompare(b.content),
  );
  return records;
}

function checkThreadBinding(records: TranscriptRecord[]): BindingViolation[] {
  const knownUserCmids = new Set(
    records
      .filter((record) => record.role === 'user')
      .map((record) => record.client_message_id)
      .filter((cmid): cmid is string => Boolean(cmid)),
  );
  const violations: BindingViolation[] = [];
  let currentUserCmid: string | undefined;

  for (const [index, record] of records.entries()) {
    if (record.role === 'user') {
      currentUserCmid = record.client_message_id;
      if (record.client_message_id && record.thread_id !== record.client_message_id) {
        violations.push({
          record_index: index,
          role: record.role,
          expected_thread_id: record.client_message_id,
          actual_thread_id: record.thread_id,
          content_preview: record.content.slice(0, 80),
        });
      }
      continue;
    }

    const responseTo = record.response_to_client_message_id;
    const expected =
      responseTo && knownUserCmids.has(responseTo) ? responseTo : currentUserCmid;
    if (expected && record.thread_id !== expected) {
      violations.push({
        record_index: index,
        role: record.role,
        expected_thread_id: expected,
        actual_thread_id: record.thread_id,
        response_to_client_message_id: responseTo,
        content_preview: record.content.slice(0, 80),
      });
    }
  }

  return violations;
}

function serializePromotableFixture(scenario: ScenarioCase) {
  return JSON.stringify(
    {
      ...scenario,
      records: materializeScenario(scenario, 'bound-at-spawn'),
      sticky_latest_user_records: materializeScenario(
        scenario,
        'sticky-latest-user',
      ),
    },
    null,
    2,
  );
}

function writePromotableFixture(
  scenario: ScenarioCase,
  records: TranscriptRecord[],
  violations: BindingViolation[],
  reason: string,
) {
  fs.mkdirSync(REGRESSION_DIR, { recursive: true });
  const fileName = `thread-binding-${reason}-seed-${scenario.seed}.fixture.json`;
  const fixturePath = path.join(REGRESSION_DIR, fileName);
  const fixture = {
    schema: REGRESSION_FIXTURE_SCHEMA,
    name: `generated-${reason}-seed-${scenario.seed}`,
    source_schema: scenario.schema,
    seed: scenario.seed,
    reason,
    turns: scenario.turns,
    completions: scenario.completions,
    records,
    expected_violations: violations,
  };
  fs.writeFileSync(fixturePath, `${JSON.stringify(fixture, null, 2)}\n`);
  return fixturePath;
}

test.describe('thread_id binding property', () => {
  test(`generated speculative-overflow transcripts preserve binding across ${ITERATIONS} cases`, () => {
    let stickyFailuresDetected = 0;

    for (let seed = 1; seed <= ITERATIONS; seed++) {
      const scenario = generateScenario(seed);
      const records = materializeScenario(scenario, 'bound-at-spawn');
      const violations = checkThreadBinding(records);
      const fixturePath =
        violations.length > 0
          ? writePromotableFixture(
              scenario,
              records,
              violations,
              'bound-at-spawn-violation',
            )
          : undefined;
      expect(
        violations,
        `generated bound-at-spawn transcript violated thread binding; wrote fixture: ${fixturePath}\npromote with:\n${serializePromotableFixture(scenario)}`,
      ).toEqual([]);

      const stickyRecords = materializeScenario(scenario, 'sticky-latest-user');
      const stickyViolations = checkThreadBinding(stickyRecords);
      if (stickyViolations.length > 0) {
        stickyFailuresDetected += 1;
      }
    }

    expect(stickyFailuresDetected).toBeGreaterThan(0);
  });

  test('promoted sticky-map regression fixtures are detected by the invariant checker', () => {
    const files = fs
      .readdirSync(REGRESSION_DIR)
      .filter((file) => file.endsWith('.fixture.json'));
    expect(files.length).toBeGreaterThan(0);

    for (const file of files) {
      const fixture = JSON.parse(
        fs.readFileSync(path.join(REGRESSION_DIR, file), 'utf8'),
      ) as RegressionFixture;
      const violations = checkThreadBinding(fixture.records);

      for (const expected of fixture.expected_violations) {
        expect(
          violations,
          `${fixture.name} should include violation ${JSON.stringify(expected)}`,
        ).toContainEqual(expect.objectContaining(expected));
      }
    }
  });
});
