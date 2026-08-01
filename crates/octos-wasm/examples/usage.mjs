// Minimal end-to-end example for the octos-wasm browser binding.
//
// Build the package first:
//   wasm-pack build --target web
// then serve this directory over http and load it from an HTML page, or adapt
// the import for a bundler (Vite/webpack) / a `--target nodejs` build.
//
// The agent itself runs server-side (`octos serve`); this file only shows the
// CLIENT-SIDE protocol modelling + wire (de)serialization the binding provides.

import init, {
  decode_ui_frame,
  encode_rpc_request,
  encode_rpc_notification,
  truncate_utf8,
  safe_filename,
  new_task_id,
  new_turn_id,
  new_client_message_id,
  new_thread_id_rooted_at,
  message_user,
  session_key_new,
  session_key_with_topic,
  ui_protocol_version,
  max_text_frame_bytes,
} from "../pkg/octos_wasm.js";

async function main() {
  await init(); // loads .wasm and installs the panic hook (start())

  console.log("ui protocol:", ui_protocol_version());
  console.log("max frame bytes:", max_text_frame_bytes());

  // --- model a session + user turn, entirely client-side ---
  const sessionId = session_key_with_topic("web", "demo", "research");
  const cmid = new_client_message_id();
  const threadId = new_thread_id_rooted_at(cmid); // == cmid
  const turnId = new_turn_id();
  const taskId = new_task_id();
  const userMsg = message_user("summarize the octos architecture");
  console.log({ sessionId, cmid, threadId, turnId, taskId, role: userMsg.role });

  // --- encode a client -> server `turn/start` request frame ---
  // Shape mirrors octos-core's `TurnStartParams`: session_id + turn_id +
  // input items (each `{ kind: "text", text }`). Sending `content` /
  // `thread_id` / `client_message_id` here instead would be rejected by the
  // server with `invalid_params`.
  const requestFrame = encode_rpc_request("req-1", "turn/start", {
    session_id: sessionId,
    turn_id: turnId,
    input: [{ kind: "text", text: userMsg.content }],
  });
  console.log("send:", requestFrame);
  // e.g. socket.send(requestFrame)

  // A minimal client -> server liveness notification. `ping` is the inbound
  // method the server recognizes; other method names come from the server's
  // advertised capabilities.
  console.log("notify:", encode_rpc_notification("ping", {}));

  // --- decode a server -> client frame ---
  const incoming =
    '{"jsonrpc":"2.0","method":"server/heartbeat","params":{"seq":7}}';
  const { kind, frame } = decode_ui_frame(incoming);
  console.log("recv:", kind, frame);

  // malformed frames reject with a JSON-RPC error object
  try {
    decode_ui_frame("}{ not json");
  } catch (err) {
    console.log("rejected:", err.code, err.message);
  }

  // --- utilities ---
  console.log(truncate_utf8("a long-ish label that overflows", 12, "…"));
  console.log(safe_filename("../notes/2026 report.md"));
}

main();
