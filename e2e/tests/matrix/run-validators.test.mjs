import { test } from "node:test";
import assert from "node:assert/strict";

import { evaluateOnboardingValidators } from "../../matrix/run.mjs";

function checksFor({ validators, stepFrames, transcriptRows = [] }) {
  return evaluateOnboardingValidators({
    scenario: { validators },
    localCtx: { profileId: "m22-profile" },
    stepFrames,
    transcriptRows,
    transcriptLog: "/tmp/transcript.jsonl",
  });
}

test("workspace validators match existing, missing, and root-escape probe shapes", () => {
  const checks = checksFor({
    validators: [
      "workspace_probe_existing_writable",
      "workspace_probe_missing_path",
      "workspace_probe_root_escape_typed",
    ],
    stepFrames: {
      probe_existing: {
        result: {
          exists: true,
          is_directory: true,
          writable: true,
          root_escape: false,
          workspace_policy: { present: false },
        },
      },
      probe_missing: {
        result: {
          exists: false,
          is_directory: false,
          root_escape: false,
        },
      },
      probe_etc: {
        result: {
          root_escape: true,
          banned_root: "etc",
        },
      },
    },
  });

  assert.deepEqual(
    checks.map((check) => check.status),
    ["passed", "passed", "passed"],
  );
});
