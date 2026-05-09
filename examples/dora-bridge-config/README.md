# Dora MCP Bridge + Mission Pipeline — Config Example

**Covers:** Dora MCP Bridge (Area 2) + Mission Pipeline (Area 5)

## The Problem

Robot hardware runs on Dora-RS dataflow graphs — camera nodes, motion planners,
gripper controllers. The LLM agent runs on octos with MCP tools. These are two
separate worlds with no connection. Without a bridge:

- **Developers write glue code for every robot tool.** Each Dora node needs a
  custom MCP adapter — parsing JSON, handling timeouts, mapping safety tiers.
  For 10 nodes, that's 10 adapters with 10 sets of bugs.
- **Multi-step missions are ad-hoc prompts.** "Go to valve V-101, inspect it,
  fix if needed, come back" is a single LLM prompt. If it fails at step 3, you
  start over. No checkpoints, no deadlines, no safety gates between steps.
- **No safety tier enforcement across the bridge.** The LLM can call any Dora
  node at any time — including full-speed actuation nodes during an observe-only
  session.

## The Solution

### Dora MCP Bridge (`dora_tool_map.json`)

A JSON config file that maps Dora-RS nodes to MCP tools **without writing code**.
Each mapping declares:
- Which Dora node/output handles the tool
- What parameters the LLM should provide
- What `safety_tier` is required (enforced by `RobotPermissionPolicy`)
- Timeout for the tool call

**Before:** Write a custom Rust adapter for each Dora node.
**After:** Add 10 lines of JSON per tool. `BridgeConfig::from_file()` loads them all.

### Mission Pipeline (`inspection_mission.dot`)

A DOT graph that defines multi-step robot missions as a DAG with safety
guarantees at each step. Each node has a `HandlerKind` that determines behavior:

| Handler | Purpose | Example |
|---------|---------|---------|
| `SensorCheck` | Read sensor, evaluate condition | "Is battery > 20%?" |
| `Motion` | Execute robot motion | "Navigate to valve P1" |
| `SafetyGate` | Require safety condition | "Force < 50N before manipulation" |
| `Codergen` | LLM-driven reasoning | "Inspect valve, decide action" |
| `Gate` | Conditional branch | "Was anomaly detected?" |

**Before:** One big LLM prompt. No checkpoints, no deadlines, no safety gates.
**After:** Each step has deadlines, invariants, and checkpoints. If the LLM
hangs on step 3, the deadline fires after 60s and skips to the next step. If
force exceeds 50N at the safety gate, the mission triggers emergency stop.

## Files

| File | What It Shows |
|------|---------------|
| `dora_tool_map.json` | 4 tool mappings with safety tiers and timeouts |
| `inspection_mission.dot` | 8-node pipeline DAG with all handler types |

## dora_tool_map.json — Tool Mappings

```
capture_valve_image  ->  camera_node:capture_request     [observe]
move_to_valve        ->  motion_planner:move_command      [safe_motion]
turn_valve           ->  gripper_node:rotate_command      [full_actuation]
read_pressure_gauge  ->  vision_node:gauge_read_request   [observe]
```

Each tool has a `safety_tier` field. When the agent is running in a `SafeMotion`
session, it can use `capture_valve_image` and `move_to_valve`, but `turn_valve`
(which requires `full_actuation`) is blocked — even though the LLM can see it
in the tool list.

### Loading the config

```rust
use octos_dora_mcp::{load_bridges, BridgeConfig};

// Load all tool mappings from the JSON config.
let config = BridgeConfig::from_file("examples/dora-bridge-config/dora_tool_map.json")?;

// `load_bridges` does TWO things:
//   1. Constructs a `DoraToolBridge` per mapping (each impls `Tool`).
//   2. Inserts each tool's name into the global `RobotToolRegistry` at its
//      declared `safety_tier`. This is what wires `group:robot:<tier>`
//      `ToolPolicy` allow/deny against these bridges. Constructing
//      `DoraToolBridge::new` directly skips the registry registration and
//      tier-based access control silently no-ops — always go through
//      `load_bridges`.
let tools = load_bridges(&config);

for tool in &tools {
    let mapping = tool.mapping();
    println!(
        "{} -> {}:{} (tier: {:?})",
        mapping.tool_name,
        mapping.dora_node_id,
        mapping.dora_output_id,
        tool.required_safety_tier(),
    );
}
```

## inspection_mission.dot — Pipeline DAG

```
preflight --> navigate --> arrival_check --> safety_gate --> inspect --> result_gate
                                                                           |
                                                                      yes / \ no
                                                                         /   \
                                                                 corrective  return_home
                                                                         \   /
                                                                      return_home
```

### Key attributes demonstrated

**Deadlines** — prevent the mission from hanging forever:
```dot
navigate [handler="Motion", deadline_secs="120", deadline_action="Abort"];
// If navigation takes > 120s, abort the mission (robot might be stuck)

inspect [handler="Codergen", deadline_secs="60", deadline_action="Skip"];
// If LLM inspection takes > 60s, skip to the next step (non-critical)
```

**Invariants** — safety conditions checked before proceeding:
```dot
preflight [handler="SensorCheck", invariant="battery_soc > 20", on_violation="Abort"];
// Don't start the mission if battery is too low to complete it

safety_gate [handler="SafetyGate", invariant="max_force_n < 50.0", on_violation="EmergencyStop"];
// If force exceeds 50N, something is wrong — trigger e-stop immediately
```

**Checkpoints** — resume a failed mission from where it left off:
```dot
navigate [checkpoint="true"];
// After arriving at the valve, save state. If the mission fails later,
// restart from here instead of navigating again.
```

### DeadlineAction options

| Action | When to use |
|--------|-------------|
| `Abort` | Critical step — mission cannot continue without it |
| `Skip` | Non-critical step — log the timeout and move to next node |
| `EmergencyStop` | Safety violation — halt all motion immediately |
