# Bundled PPTX resources

Release slice: ship the registered `skill-evolve` executable and the complete
PPTX resource skill to clean Octos / OctosCode installations. No deck content,
generated images, evaluation workspaces, credentials, or host virtual environment
belongs in the release.

## Packaging and discovery

The release build compiles `skill-evolve` and `bundle-release.sh` requires its
binary. The local bundle builder uses the same shipping list. Its existing
credential and tool gates remain in force; packaging does not grant permissions.

`octos-agent/resource-skills/` is the canonical source of the PPTX instructions,
references, Python helpers, dependency versions, and the companion parallel-work
skill. These files are embedded into the Rust library at build time. A binary
therefore retains its exact resource version through the normal OctosCode runtime
download/install path, including platforms whose installer only copies executables.

The account-scoped skill loader materializes resources under
`<profile-data-dir>/bundled-resource-skills/`. They are discoverable without a
provider key, a previous workspace, or Python. Full instructions are loaded on
demand, not injected into every model request. Profile/global user skills have
precedence; normal skill selection filters can disable bundled resources.
Both serve/OctosCode and gateway sessions add these account-local resource roots
to their existing read-only skill zones. Thus `read_file` can open the catalog
paths without granting writes to the bundle or reads of unrelated account files.

Extraction uses a per-account lock and staged directory replacement. Unchanged
resources are reused. A changed or incomplete managed bundle is replaced after
the previous directory is retained under `skill-backups/`. Custom skills under
`skills/` are never overwritten. Symlinked managed destinations are rejected.

## Runtime requirements

Scripts are Python, not Rust implementations of PowerPoint. On first PPTX use,
the agent runs the included `scripts/setup_runtime.py --workspace <path> --check`.
`--install` creates a workspace virtual environment and installs the pinned
Python requirements. It does not run during ordinary skill discovery/startup.
LibreOffice and suitable fonts are host prerequisites for rendering; missing
prerequisites are reported explicitly. Image generation needs a separately
configured image service or an available authenticated image tool. Neither model
nor image credentials are embedded, copied, or printed by setup.

## Acceptance

- `scripts/test-bundle-release.sh`: checksum, presence of `skill-evolve`, and
  rejection of an incomplete build.
- `should_discover_complete_pptx_resources_on_clean_install`: discovery from an
  empty account directory, real paths for scripts and references.
- Resource bootstrap tests: upgrade, backup, no-op reuse, deleted-resource repair,
  profile/global overrides, filtering, concurrent extraction and symlink rejection.
- Resource package tests: required files and relative references, no host paths
  or private artifacts, setup/check behavior and release build-list consistency.
- Clean-directory executable smoke: send the packaged skill in the model catalog,
  read it through the actual scoped `read_file` tool, then run its installed Python
  builder and structural checker on a small synthetic deck.

Run the release-binary smoke with a localhost fixture provider (no model/image
credentials or API usage):

```sh
python3.12 scripts/tests/test-bundled-pptx-runtime.py --binary target/release/octos
# Also install the Python dependencies into the fixture workspace, construct an
# editable slide, and verify text survives actual LibreOffice PDF/PNG rendering:
python3.12 scripts/tests/test-bundled-pptx-runtime.py --binary target/release/octos --render
```

Use `OUP_TEST_OUTPUT_DIR` to retain the requests, build/render logs, PPTX, and
`smoke.json` in a chosen new directory. `--render` needs the host prerequisites
above and may download Python dependencies; it does not call an image service.
The standard `milestone-ci.sh oup-runtime` lane runs the catalog/tool-read smoke
without `--render`, so continuous integration needs no image credits or renderer.

These checks validate packaging and tool availability. They do not certify the
visual quality of a model-produced deck or PowerPoint application compatibility.
