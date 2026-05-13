# Skill Fleet Deployment

This document covers operator-side deployment of skill packages onto an
octos fleet (e.g. the mini1-5 cluster). It supersedes the legacy
`mofa-skills/scripts/deploy-mini.sh` raw-scp flow.

Related docs: [`STRICT_ACCOUNT_SCOPED_SKILLS.md`](./STRICT_ACCOUNT_SCOPED_SKILLS.md)
for the architectural decision to make skill installs per-profile only.

## Why per-profile only

Customer skills resolve from exactly one place:

```
~/.octos/profiles/<account-or-subaccount>/data/skills/
```

No parent-profile inheritance, no project-level fallback, no global
customer-skill layer. See `STRICT_ACCOUNT_SCOPED_SKILLS.md` for the rationale.
The global `~/.octos/skills/` directory is being deprecated; a parallel change
in the agent loader (`octos-agent/src/plugins/loader.rs`) emits a deprecation
warning when it sees that path on disk.

## What replaces `deploy-mini.sh`

`scripts/fleet-install-skills.sh` is the single canonical entry point for
pushing mofa-skills onto the fleet. Differences from the old script:

| Concern                | `deploy-mini.sh` (legacy)            | `fleet-install-skills.sh` (new)                              |
|------------------------|--------------------------------------|---------------------------------------------------------------|
| Transport              | raw `scp` of each file               | `rsync` to staging + `octos skills install <staging-path>`    |
| sha256 verification    | none                                 | yes, via `manage_skills::download_binary` when manifest binaries are declared |
| Install path           | hard-coded `~/.octos/profiles/dspfac/data/skills/` | resolved server-side by `--profile <id>`                      |
| Multi-profile          | one profile per run                  | iterates every profile on the host (or explicit `--profile`)  |
| Multi-host             | one host per invocation              | full fleet by default; subset via `--host`                    |
| Failure isolation      | aborts on first failure              | logs and continues; summary table at the end                  |
| Idempotency            | always re-copies                     | re-runnable; uses `--force` by default for deterministic state |
| Dry-run                | none                                 | `--dry-run` prints every command without execution            |

## How `fleet-install-skills.sh` works

For each (host, profile, skill) triple, the script:

1. `rsync -az --delete <mofa-skills>/<skill>/ <host>:/tmp/octos-fleet-install-staging/<skill>/`
2. `ssh <host> "OCTOS_PROFILE_ID=<p> /Users/cloud/.octos/bin/octos skills --profile <p> install /tmp/octos-fleet-install-staging/<skill> --force"`
3. Records OK / FAIL with the tail of stderr.
4. Cleans up staging at the end of the host.

The `octos skills install <local-path>` path
(`crates/octos-cli/src/commands/skills.rs::install_from_local`) then:

- Resolves the target directory via `resolve_profile_skills_dir` —
  strictly per-account, per `STRICT_ACCOUNT_SCOPED_SKILLS.md`.
- Copies the staged dir into the per-profile `data/skills/<skill>/`.
- Runs `maybe_install_binary` — which, when the skill's `manifest.json`
  declares a `binaries.<platform>.url` + `sha256`, downloads and
  sha256-verifies the binary. Falls back to `cargo build --release` if
  the skill ships source.
- Writes `.source` for future `skills update` calls.

The result: every install goes through the same code path the runtime
`manage_skills` tool uses. `scp` is no longer in the picture.

## CLI

```
scripts/fleet-install-skills.sh [OPTIONS]

  --host LIST          Comma/space hosts (default: 69.194.3.{128,129,203,66,19})
  --profile LIST       Comma-separated profile IDs (default: enumerate per host)
  --skill LIST         Comma-separated skill names (default: all mofa-* with
                       SKILL.md+manifest.json in MOFA_SKILLS_DIR)
  --mofa-dir PATH      mofa-skills checkout (default: ~/home/mofa-skills)
  --remote-bin PATH    octos binary on remote (default: /Users/cloud/.octos/bin/octos)
  --remote-user USER   SSH user (default: cloud)
  --no-force           Skip skills that already exist instead of overwriting
  --dry-run            Print commands without executing
  --verbose, -v        Echo each command before running
  --help, -h           Show usage
```

Environment overrides: `OCTOS_FLEET_HOSTS`, `MOFA_SKILLS_DIR`,
`OCTOS_REMOTE_BIN`, `OCTOS_REMOTE_USER`, `OCTOS_REMOTE_STAGING`.

## Migration: hosts that already have `~/.octos/skills/` populated

If a host has the legacy global directory, copy each skill into every
profile's `data/skills/`, then remove the global dir. The loader will
emit a deprecation warning on every startup until this is done.

```sh
# On the host:
for skill in ~/.octos/skills/*/; do
    name=$(basename "$skill")
    for profile_data in ~/.octos/profiles/*/data; do
        mkdir -p "$profile_data/skills"
        cp -R "$skill" "$profile_data/skills/$name"
    done
done

# Once verified the runtime sees the per-profile copies:
rm -rf ~/.octos/skills/
```

After running `fleet-install-skills.sh` from your dev box, the per-profile
copies become the canonical source — at that point you can purge the
global dir without copying first.

## Verification after deploy

For each host:

```sh
ssh cloud@<host> 'ls -la ~/.octos/profiles/*/data/skills/'
```

You should see fresh mtimes on every (profile, skill) pair the script
touched. The daemon log (`launchctl print gui/$(id -u)/io.octos.serve`,
or wherever `io.octos.serve` writes its stderr) must NOT show
`loaded unverified plugin` warnings — those only stop appearing after
mofa-skills ships sha256-equipped manifests, which is the parallel PR
tracked separately.

## Tests

`scripts/tests/test-fleet-install-skills.sh` runs entirely offline. It
exercises the dry-run plan, error paths (unknown skill, missing
mofa-dir), and the `--no-force` / env-override flags. Invoke directly:

```sh
bash scripts/tests/test-fleet-install-skills.sh
```

Also runs `shellcheck` against `scripts/fleet-install-skills.sh`.

## Deprecation of `mofa-skills/scripts/deploy-mini.sh`

The legacy script in `mofa-skills/scripts/deploy-mini.sh` now errors out
with a redirect message pointing at `fleet-install-skills.sh`. The legacy
behavior is gated behind `OCTOS_DEPRECATED_SCP=1` for emergencies; do not
rely on it.
