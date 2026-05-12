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
guarantees at each step. Each node has a `HandlerKind` that determines behavior.

The current `octos-pipeline` DOT parser recognises this set of handlers:

| Handler (parser keyword) | Purpose | Example use in this graph |
|---|---|---|
| `codergen` | LLM-driven reasoning over allowed tools | Inspect valve, plan corrective action |
| `gate` | Conditional branch on a runtime condition | "Was anomaly detected?" |
| `shell` | Run a shell command | (not used here) |
| `noop` | Pass-through marker node | (not used here) |
| `parallel` / `dynamic_parallel` | Fan-out across workers | (not used here) |

Higher-level robotics handlers — `SensorCheck`, `Motion`, `SafetyGate` —
appear in design discussions but are **not yet implemented in the parser /
executor**. The example uses `codergen` as the stand-in: the LLM is given the
appropriate observe-tier or safe-motion bridge tool and prompted to perform
the check or motion. When the dedicated handlers land, swap the
`handler="codergen"` attributes for `handler="sensor_check"` /
`handler="motion"` / `handler="safety_gate"` (and add the parser keywords +
handler implementations in the same change).

**Before:** One big LLM prompt. No checkpoints, no deadlines, no separation
of concerns.
**After:** Each step is its own node with its own deadline + checkpoint, and
restricted to a tier-appropriate subset of bridge tools. If the LLM hangs on
inspection, the deadline fires after 60s and skips to the next step.

## Files

| File | What It Shows |
|------|---------------|
| `dora_tool_map.json` | 4 tool mappings with safety tiers and timeouts |
| `inspection_mission.dot` | 9-node pipeline DAG using `codergen` / `gate` / `noop` |

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

**Deadlines** — prevent the mission from hanging forever (parser-supported
attributes + lowercase action keywords):
```dot
navigate [handler="codergen", deadline_secs="120", deadline_action="abort"];
// If navigation takes > 120s, abort the mission (robot might be stuck).

inspect [handler="codergen", deadline_secs="60", deadline_action="skip"];
// If LLM inspection takes > 60s, skip to the next step (non-critical).
```

**Invariants / safety gates** — modelled here with the `gate` handler. The
parser does not yet recognise `invariant=...` / `on_violation=...` as
attributes, so safety conditions are encoded in the gate's `prompt` (a
predicate against the upstream outcome) and routing is encoded on the
outgoing edges' `condition=` attribute:
```dot
safety_gate [handler="gate", prompt="outcome.contains(\"force_ok\")"];

safety_gate -> inspect        [condition="outcome.status == \"pass\""];
safety_gate -> emergency_stop [condition="outcome.status == \"fail\""];
```
The supported predicate language is `outcome.status == "pass"|"fail"`,
`outcome.contains("...")`, plus `&& || !` combinators (see
`crates/octos-pipeline/src/condition.rs`). Without a `prompt`, `gate` nodes
default to `"true"` (always Pass), and without `condition=` on the
outgoing edges the executor falls back to label substring matching against
outcome content — so both halves are required for real branching.

**Checkpoints** — resume a failed mission from where it left off:
```dot
navigate [handler="codergen", checkpoint="true"];
// After arriving at the valve, save state. If the mission fails later,
// restart from here instead of navigating again.
```

### deadline_action keywords

The parser accepts these lowercase values for `deadline_action`:

| Keyword | When to use |
|---|---|
| `abort` | Critical step — mission cannot continue without it. |
| `skip` | Non-critical step — log the timeout and move on. |
| `escalate` | Hand the timeout to the parent / supervisor handler. |
| `retry:<N>` | Retry the node up to N attempts (e.g. `retry:3`). |

There is no `EmergencyStop` action keyword today; trigger e-stop semantics by
routing through a `gate` handler whose `false` branch fires the appropriate
emergency-tier bridge tool, or extend the parser with a new keyword in the
same change that adds the executor support.
