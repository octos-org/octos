//! # Pick-and-Place Lifecycle — Hardware Startup, Shutdown, and Recovery
//!
//! ## The Problem
//!
//! Industrial robots need a strict startup/shutdown sequence: check air
//! pressure, power on servo drives, home axes, verify sensors — in that
//! exact order. Skip the preflight check and the gripper has no air? The
//! robot drops a part on the conveyor. Skip homing? The arm doesn't know
//! where it is and crashes into the workcell wall on its first move.
//!
//! Shutdown is equally critical. Power off servos before parking the arm?
//! It falls under gravity. Skip the gripper release? It's still clamping a
//! part when maintenance tries to move it manually.
//!
//! **Before this feature:**
//! - Startup/shutdown is ad-hoc shell scripts or manual operator steps.
//! - No retry logic — if the servo driver fails once on power-on (common
//!   with older hardware), the whole startup fails and someone has to
//!   SSH in to retry.
//! - No critical/non-critical distinction — a flaky encoder check aborts
//!   the entire startup even though the robot can run fine without it.
//! - No emergency path — graceful shutdown takes 30 seconds; you need a
//!   2-second path for e-stop situations.
//! - **No sandboxing.** Every shell step ran with the full parent env,
//!   including injection vectors like `LD_PRELOAD` and `BASH_ENV`, and
//!   nothing stopped a typo'd `rm -rf /` from running.
//!
//! **After this feature (RP02):**
//! - [`HardwareLifecycle`] declares 5 ordered phases as data, not code.
//! - Each [`LifecycleStep`] has timeout_ms, retries, and critical flag.
//! - [`LifecycleExecutor`] runs steps in order, **through a `Sandbox`
//!   trait**, with `BLOCKED_ENV_VARS` scrubbed and `is_safe_shell_command`
//!   vetoing obvious footguns before dispatch.
//! - Timeouts kill the **entire process group**, so background children
//!   spawned by the shell are reaped too — no zombies.
//! - Critical failures abort the phase; non-critical failures log and
//!   continue.
//! - Emergency shutdown is a separate fast path with short timeouts.
//!
//! ## Scenario
//!
//! A pick-and-place workcell with a 6-axis arm, pneumatic gripper, and
//! conveyor. The lifecycle covers: preflight checks -> initialization ->
//! ready verification -> normal operation -> graceful shutdown (or
//! emergency stop).
//!
//! ```bash
//! cargo run --example pick_and_place_lifecycle -p octos-plugin
//! ```

use octos_plugin::{HardwareLifecycle, LifecycleExecutor, LifecyclePhase, LifecycleStep};

/// Helper to construct a lifecycle step. In production, these come from
/// the plugin's `manifest.json` under `hardware_lifecycle`.
fn step(
    label: &str,
    command: &str,
    timeout_ms: u64,
    retries: u32,
    critical: bool,
) -> LifecycleStep {
    LifecycleStep {
        label: label.to_string(),
        command: command.to_string(),
        timeout_ms,
        retries,
        critical,
    }
}

#[tokio::main]
async fn main() {
    // ── Step 1: Declare the lifecycle phases ──
    //
    // Each phase is an ordered list of steps. The executor runs them top
    // to bottom. Notice the design choices per phase:
    //
    // - PREFLIGHT: Check prerequisites. Retries on flaky checks (air
    //   supply sensor sometimes reads zero on first poll). Non-critical
    //   steps (conveyor encoder) don't abort the whole startup.
    //
    // - INIT: Bring hardware up in dependency order. Servo drives MUST
    //   succeed (critical=true, retries=2) because everything depends on
    //   them. Conveyor is non-critical — the arm can pick/place without it.
    //
    // - READY_CHECK: Final verification before handing control to the
    //   agent. All critical — if the force sensor isn't zeroed, contact
    //   detection won't work and the robot could crush parts.
    //
    // - SHUTDOWN: Reverse of init. Park arm first (while servos still
    //   powered), then release gripper, then power off. Order matters.
    //
    // - EMERGENCY_SHUTDOWN: 2-second budget. Stop everything NOW. No
    //   retries, short timeouts. This is the e-stop path.
    let lifecycle = HardwareLifecycle {
        preflight: vec![
            step(
                "Check gripper air supply",
                "echo 'Air pressure: 6.2 bar OK'",
                5_000,
                1,
                true,
            ),
            step(
                "Check camera connection",
                "echo 'Camera /dev/video0 ready'",
                5_000,
                0,
                true,
            ),
            // Non-critical: conveyor encoder is nice-to-have, not essential
            step(
                "Check conveyor encoder",
                "echo 'Encoder pulses: nominal'",
                5_000,
                0,
                false,
            ),
        ],
        init: vec![
            // Retries=2: servo drivers sometimes need a second power-on
            step(
                "Power on servo drives",
                "echo 'Servo drives powered'",
                10_000,
                2,
                true,
            ),
            // Homing is slow but must succeed — arm position is unknown otherwise
            step(
                "Home all axes",
                "echo 'Homing complete: 6 axes at zero'",
                30_000,
                1,
                true,
            ),
            step("Open gripper", "echo 'Gripper opened'", 5_000, 0, true),
            // Non-critical: robot can operate without conveyor
            step(
                "Start conveyor",
                "echo 'Conveyor running at 0.2 m/s'",
                5_000,
                0,
                false,
            ),
        ],
        ready_check: vec![
            step(
                "Verify joint limits",
                "echo 'All joints within limits'",
                5_000,
                0,
                true,
            ),
            step(
                "Verify force sensor zero",
                "echo 'Force sensor zeroed: [0.01, 0.00, 0.02]'",
                5_000,
                0,
                true,
            ),
            step(
                "Verify workspace clear",
                "echo 'Workspace clear — no obstacles detected'",
                5_000,
                0,
                true,
            ),
        ],
        shutdown: vec![
            // Order matters: park arm BEFORE powering off servos
            step(
                "Park arm at home",
                "echo 'Arm parked at home position'",
                15_000,
                1,
                true,
            ),
            step("Open gripper", "echo 'Gripper released'", 5_000, 0, true),
            step("Stop conveyor", "echo 'Conveyor stopped'", 5_000, 0, false),
            // Last: power off servos after arm is safely parked
            step(
                "Power off servo drives",
                "echo 'Servos powered off'",
                10_000,
                0,
                true,
            ),
        ],
        emergency_shutdown: vec![
            // 2-second timeout, no retries — this is the e-stop path
            step(
                "Emergency stop all axes",
                "echo 'E-STOP: all axes halted'",
                2_000,
                0,
                true,
            ),
            step(
                "Vent gripper pressure",
                "echo 'Gripper pressure vented'",
                2_000,
                0,
                true,
            ),
        ],
    };

    // ── Step 2: Build the executor ──
    //
    // LifecycleExecutor wraps every step through the Sandbox trait, strips
    // BLOCKED_ENV_VARS before spawn, screens commands via SafePolicy, and
    // kills the child process group on timeout. For this demo we use
    // NoSandbox (pass-through) because production sandbox backends
    // (sandbox-exec, bwrap) are workstation-specific. In a real plugin
    // you'd pass a concrete Sandbox tuned for your platform.
    let executor = LifecycleExecutor::with_no_sandbox();

    // ── Step 3: Run the normal startup sequence ──
    //
    // run_phase() handles the complexity:
    // - Runs steps in order
    // - Retries failed steps up to step.retries times
    // - Aborts on critical failure (returns partial PhaseResult)
    // - Logs and continues on non-critical failure
    // - Times out individual steps (prevents hanging on dead hardware)
    let startup_phases = [
        (LifecyclePhase::Preflight, &lifecycle.preflight),
        (LifecyclePhase::Init, &lifecycle.init),
        (LifecyclePhase::ReadyCheck, &lifecycle.ready_check),
    ];

    println!("╔══════════════════════════════════════════╗");
    println!("║  WORKCELL STARTUP SEQUENCE               ║");
    println!("╚══════════════════════════════════════════╝\n");

    for (phase, steps) in startup_phases {
        println!("── Phase: {phase} ({} steps) ──", steps.len());
        let result = executor.run_phase(phase, steps).await;

        if result.success {
            println!(
                "  -> PASSED ({}/{} steps completed)\n",
                result.steps_completed, result.steps_total
            );
        } else {
            // In production, a failed startup means: do NOT hand control to
            // the agent. The robot is not safe to operate.
            println!(
                "  -> FAILED at step {}/{}: {}",
                result.steps_completed + 1,
                result.steps_total,
                result.error.as_deref().unwrap_or("unknown error")
            );
            println!("  -> Robot NOT safe to operate. Fix the issue and retry.\n");
            return;
        }
    }

    println!("All startup phases passed. Robot is ready for agent control.\n");

    // ── Step 4: Normal graceful shutdown ──
    //
    // Called when the operator ends the session or the agent completes
    // its task. Steps run in reverse dependency order (park arm -> release
    // gripper -> stop conveyor -> power off servos).
    println!("╔══════════════════════════════════════════╗");
    println!("║  GRACEFUL SHUTDOWN                       ║");
    println!("╚══════════════════════════════════════════╝\n");

    println!("── Phase: shutdown ({} steps) ──", lifecycle.shutdown.len());
    let shutdown_result = executor
        .run_phase(LifecyclePhase::Shutdown, &lifecycle.shutdown)
        .await;
    println!(
        "  -> {} ({}/{} steps completed)\n",
        if shutdown_result.success {
            "COMPLETED"
        } else {
            "PARTIAL"
        },
        shutdown_result.steps_completed,
        shutdown_result.steps_total,
    );

    // ── Step 5: Emergency shutdown (separate fast path) ──
    //
    // This is NOT the normal shutdown. This fires when:
    // - E-stop button pressed
    // - Force limit exceeded (robot hit something)
    // - Heartbeat stalled (agent frozen)
    // - Operator triggers emergency via API
    //
    // Key differences from graceful shutdown:
    // - 2-second timeouts (vs 10-15 seconds)
    // - No retries (vs 1-2 retries)
    // - Minimal steps (vs full sequence)
    // - Runs INSTEAD OF graceful shutdown, not after it
    println!("╔══════════════════════════════════════════╗");
    println!("║  EMERGENCY SHUTDOWN (e-stop path)        ║");
    println!("╚══════════════════════════════════════════╝\n");

    println!(
        "── Phase: emergency_shutdown ({} steps, 2s timeout each) ──",
        lifecycle.emergency_shutdown.len()
    );
    let estop_result = executor
        .run_phase(
            LifecyclePhase::EmergencyShutdown,
            &lifecycle.emergency_shutdown,
        )
        .await;
    println!(
        "  -> {} ({}/{} steps completed)",
        if estop_result.success {
            "COMPLETED"
        } else {
            "PARTIAL"
        },
        estop_result.steps_completed,
        estop_result.steps_total,
    );

    println!("\nPick-and-place lifecycle demo complete.");
    println!("In production, these phases are declared in manifest.json and run");
    println!("automatically by the plugin system at session start/end.");
}
