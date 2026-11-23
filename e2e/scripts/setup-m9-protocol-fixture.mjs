import { mkdirSync, writeFileSync } from "node:fs";
import { resolve, join } from "node:path";

// Match the canonical _main:api session keys used by the protocol specs.
// No LLM selection: OCTOS_M9_PROTOCOL_FIXTURES supplies deterministic turns.
const dataDir = resolve(process.argv[2]);
const profileDir = join(dataDir, "profiles", "_main-data");
mkdirSync(profileDir, { recursive: true });
writeFileSync(join(dataDir, "profiles", "_main.json"), JSON.stringify({
  id: "_main",
  name: "M9 protocol fixture",
  enabled: true,
  // Below the fixture root, never the root itself: the serve chats root is
  // <data-dir>/chats and the server-root safety check refuses a profile data
  // dir that overlaps it (#3326).
  data_dir: profileDir,
  config: {},
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
}));
writeFileSync(join(dataDir, "serve.json"), JSON.stringify({ allow_admin_shell: true }));
