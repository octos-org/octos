# Native ARC Agent Tasks over MCP

Octos can execute a versioned task compiled by the Agentic Requirement
Compiler (ARC) without flattening the task into a free-form prompt. The
integration reuses the existing `run_octos_session` MCP tool and adds an
optional `input.arc_task` contract.

## Capability discovery

ARC first initializes `octos mcp-serve` and calls `tools/list`. A compatible
Octos build advertises this nested field:

```text
run_octos_session.inputSchema
└── properties.input.properties.arc_task
    └── properties.schema.const = "arc.agent-task.v1"
```

This explicit capability check prevents an older Octos binary from silently
running the compiled task through the legacy prompt path.

## Request shape

`contract` remains the Octos workspace contract name. ARC normally uses
`coding`; the versioned ARC package is carried separately:

```json
{
  "contract": "coding",
  "input": {
    "arc_task": {
      "schema": "arc.agent-task.v1",
      "task_id": "REQ-1:DESIGN:InterfaceDesigner",
      "stage": "InterfaceDesigner",
      "backend_agent_name": "interface_designer",
      "node_id": "REQ-1",
      "phase": "DESIGN",
      "app_type": "web",
      "workspace_root": "/absolute/output/workspace",
      "requirement_path": "/absolute/output/workspace/requirements/requirements.yaml",
      "thread_id": "REQ-1:DESIGN:InterfaceDesigner",
      "test_type": "",
      "system_prompt": "You are the interface designer.",
      "message": "Design interfaces for requirement REQ-1.",
      "response_schema": {
        "type": "object",
        "required": ["summary"],
        "properties": {
          "summary": {"type": "string"}
        }
      },
      "inputs": {
        "requirement": {"id": "REQ-1"}
      },
      "acceptance": {
        "response_schema_required": true,
        "artifact_kind": "interface_design"
      },
      "skills": []
    },
    "expected_artifact": ".arc/delegated/InterfaceDesigner-REQ-1.json",
    "artifact_name": "arc-stage-result"
  }
}
```

## Native mapping

Octos validates the package before constructing an LLM provider or starting
the agent loop:

| ARC field | Octos execution behavior |
| --- | --- |
| `system_prompt` | Installed through `Agent::with_system_prompt`; it is not copied into user content |
| `message` | Stored as the structured custom task instruction |
| `requirement_path` | Preserved in the structured task so the agent can trace the compiled requirement source |
| `inputs` | Preserved as structured task parameters |
| `acceptance` | Preserved as mandatory completion criteria |
| `response_schema` | Sent to the agent and used to validate the final JSON artifact |
| task identity fields | Preserved for traceability in the custom task |
| `expected_artifact` | Required workspace-relative delivery path |

The declared `skills` are preserved in the task parameters but are not
automatically installed into Octos. Skill installation and trust remain an
operator responsibility.

## Validation and safety

Native ARC execution fails closed when:

- the schema is not exactly `arc.agent-task.v1`;
- a required field is missing, empty, or has the wrong JSON type;
- `acceptance.response_schema_required` is true while `response_schema` is null;
- `workspace_root` does not resolve to the `mcp-serve --cwd` workspace;
- `expected_artifact` is absolute or contains a parent traversal;
- the resolved artifact follows a symlink outside the workspace;
- a declared response schema does not match the JSON artifact.
- the agent returns an unsuccessful `TaskResult`, even if an artifact from an
  earlier attempt already exists at `expected_artifact`.

ARC response validation supports the schema constructs emitted by its current
Pydantic models: `$ref`/`$defs`, `type`, `required`, `properties`, `items`,
`anyOf`, `oneOf`, `enum`, `const`, and `additionalProperties: false`.

Input errors use the `arc_task_invalid:` prefix. Artifact validation errors use
`artifact_schema_invalid:`. Both prevent a Ready outcome.

## Backward compatibility

Calls without `input.arc_task` retain the existing `input.prompt`,
`expected_artifact`, and `artifact_name` behavior. Octos continues to expose
exactly one MCP tool, so existing orchestrators are unaffected.

## Verification

Run the focused contract and dispatch tests:

```bash
cargo test -p octos-agent --test mcp_server arc_agent_task_v1 --no-default-features
cargo test -p octos-cli --test mcp_serve_integration arc_agent_task
cargo test -p octos-cli --test mcp_serve_integration legacy_prompt_session_remains_compatible_without_arc_task
agent-spec lint specs/arc-agent-task-v1-mcp.spec.md --min-score 0.9
```
