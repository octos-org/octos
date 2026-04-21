//! Robot-integrator demo of the RP03 domain-hook pattern.
//!
//! This example shows how to veto a dispatched sub-task based on live robot
//! telemetry **without** adding any robot-specific `HookEvent` variants to the
//! core agent. The pattern is:
//!
//! 1. Implement `HookPayloadEnricher` in integrator code. The trait has one
//!    synchronous method that reads the latest sensor snapshot from an
//!    `Arc`-shared state and attaches it to `HookPayload.domain_data`.
//! 2. Register the enricher on the shared `HookExecutor` via
//!    `HookExecutor::with_enricher(Arc::new(my_enricher))`.
//! 3. Write a before-hook shell script that reads the payload from stdin,
//!    parses `domain_data`, and exits 1 to deny.
//!
//! Run with: `cargo run -p octos-agent --example robot_domain_hook`
//!
//! Expected output (last line):
//!
//! ```text
//! hook decision: Deny("force 55.2 N exceeds 40 N limit")
//! ```

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::Mutex;

use octos_agent::{
    HookConfig, HookEvent, HookExecutor, HookPayload, HookPayloadEnricher, HookResult,
};

/// A snapshot of the robot's physical state, refreshed by an external polling
/// task in a real integration. Here we just hold fixed values for the demo.
#[derive(Debug, Clone, Copy)]
struct RobotSnapshot {
    /// End-effector force magnitude in Newtons.
    force_n: f64,
    /// Emergency-stop engaged.
    estop: bool,
    /// Latest pose within the configured safe workspace.
    workspace_in_bounds: bool,
}

/// Real integrators would back this with `Arc<RwLock<_>>` updated by a sensor
/// polling task. For the demo we use a `Mutex` and a fixed value.
struct RobotSensorBus {
    latest: Mutex<RobotSnapshot>,
}

impl RobotSensorBus {
    fn current(&self) -> RobotSnapshot {
        *self.latest.lock().unwrap()
    }
}

/// Enricher that reads the latest robot snapshot and attaches it to every hook
/// payload as `domain_data`. The hook script filters on these fields.
struct RobotSensorEnricher {
    bus: Arc<RobotSensorBus>,
}

impl HookPayloadEnricher for RobotSensorEnricher {
    fn enrich(&self, _event: &HookEvent, payload: &mut HookPayload) {
        let snap = self.bus.current();
        payload.domain_data = Some(serde_json::json!({
            "force_n": snap.force_n,
            "estop": snap.estop,
            "workspace_in_bounds": snap.workspace_in_bounds,
        }));
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // 1. Set up the shared sensor bus. In a real robot integration this would
    //    be populated by a tokio task polling the robot controller.
    let bus = Arc::new(RobotSensorBus {
        latest: Mutex::new(RobotSnapshot {
            force_n: 55.2,
            estop: false,
            workspace_in_bounds: true,
        }),
    });

    // 2. Drop the before-hook script to a temp path. Integrators usually ship
    //    this under `~/.octos/hooks/` and point to it via config.json.
    let dir = tempfile::tempdir()?;
    let script_path = dir.path().join("robot_guard.sh");
    let script = r#"#!/bin/sh
# Before-hook for BeforeSpawnVerify. Reads stdin JSON and denies motion if:
#   - e-stop is engaged, OR
#   - force_n exceeds 40 N, OR
#   - workspace_in_bounds is false.
payload="$(cat)"

estop=$(printf '%s' "$payload" \
    | sed -n 's/.*"estop":[[:space:]]*\(true\|false\).*/\1/p' \
    | head -n1)
force=$(printf '%s' "$payload" \
    | sed -n 's/.*"force_n":[[:space:]]*\([0-9][0-9]*\(\.[0-9]*\)*\).*/\1/p' \
    | head -n1)
bounds=$(printf '%s' "$payload" \
    | sed -n 's/.*"workspace_in_bounds":[[:space:]]*\(true\|false\).*/\1/p' \
    | head -n1)

if [ "$estop" = "true" ]; then
    printf "e-stop engaged"
    exit 1
fi
if [ "$bounds" = "false" ]; then
    printf "end-effector outside safe workspace"
    exit 1
fi
if [ -n "$force" ]; then
    violates=$(awk -v f="$force" 'BEGIN { print (f > 40) ? 1 : 0 }')
    if [ "$violates" = "1" ]; then
        printf "force %s N exceeds 40 N limit" "$force"
        exit 1
    fi
fi
exit 0
"#;
    write_exec(&script_path, script)?;

    // 3. Build a HookExecutor that fires the guard on BeforeSpawnVerify and
    //    registers the robot sensor enricher. This is the pattern integrators
    //    wire into `ChatSession::with_hooks` / gateway construction.
    let executor = HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![script_path.to_string_lossy().to_string()],
        timeout_ms: 5000,
        tool_filter: vec![],
    }])
    .with_enricher(Arc::new(RobotSensorEnricher { bus: bus.clone() }));

    // 4. Simulate the agent reaching its pre-motion verify checkpoint: the
    //    spawn tool would build a BeforeSpawnVerify payload and call run().
    let payload = HookPayload::before_spawn_verify(
        "task-move-arm",
        "Pick object at (0.42, 0.11, 0.08)",
        "parent-session",
        "child-session",
        Some("robot"),
        Some("pre_motion"),
        Some("motion candidate ready"),
        vec![],
        None,
    );
    let result = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;

    println!("hook decision: {result:?}");

    // Demonstrate the allow path by lowering the force reading.
    {
        let mut snap = bus.latest.lock().unwrap();
        snap.force_n = 12.0;
    }
    let payload = HookPayload::before_spawn_verify(
        "task-move-arm-2",
        "Place object",
        "parent-session",
        "child-session",
        Some("robot"),
        Some("pre_motion"),
        Some("safer motion candidate"),
        vec![],
        None,
    );
    let result2 = executor.run(HookEvent::BeforeSpawnVerify, &payload).await;
    println!("hook decision (after relaxing force): {result2:?}");

    // Exit nonzero if the deny path did not fire as expected so CI catches
    // regressions of the pattern.
    match result {
        HookResult::Deny(_) => Ok(()),
        other => Err(eyre::eyre!(
            "expected Deny for 55.2 N over-limit, got {other:?}"
        )),
    }
}

fn write_exec(path: &std::path::Path, contents: &str) -> eyre::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(contents.as_bytes())?;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}
