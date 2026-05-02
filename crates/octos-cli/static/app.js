/*
 * octos chat SPA — UI Protocol v1 over WebSocket (PR J)
 *
 * This is the in-tree reference SPA. PR J migrates the live path from
 * legacy `POST /api/chat` + SSE to UI Protocol v1 JSON-RPC over WebSocket
 * (`/api/ui-protocol/ws`). REST endpoints stay live as a fallback when
 * the WS handshake fails (corporate proxies, older CDN configs).
 *
 * Design references:
 *   - /tmp/pr-j-design.md            (migration table + scope)
 *   - api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md
 *   - e2e/lib/m9-ws-client.ts        (TS reference client — mirrored here)
 *   - crates/octos-web/src/state/__tests__/lib/fixed-reducer.ts
 *     (the executable reducer contract — this SPA's reducer matches it)
 *
 * Architecture:
 *
 *   ┌──────────── boot decision tree ────────────┐
 *   │  loadSessionsRest()  -> populate sidebar   │
 *   │  attemptWsHandshake()                       │
 *   │   ├─ ok  -> LIVE-WS mode                    │
 *   │   │       (session/hydrate -> render,       │
 *   │   │        notifications drive reducer)     │
 *   │   └─ fail -> LEGACY-REST mode               │
 *   │             (fetch SSE via POST /api/chat)  │
 *   └─────────────────────────────────────────────┘
 *
 * The reducer is keyed by `turn_id` (typed UUIDv7 from the server) — not
 * by sticky thread or by message position. That is THE structural fix:
 * a late background tool result CANNOT bind to the wrong bubble because
 * the envelope IS the binding.
 */

(function () {
  "use strict";

  // ---- constants ---------------------------------------------------------

  var TOKEN_STORAGE_KEY = "octos_token";
  var SESSION_STORAGE_KEY = "octos_current_session";
  var LIVE_WS = "ws-live";
  var LEGACY_REST = "rest-fallback";
  var WS_HANDSHAKE_TIMEOUT_MS = 3000;
  var WS_RECONNECT_BACKOFF_MS = [250, 500, 1000, 2000, 5000];

  // UI Protocol v1 method names (mirrors octos-core::ui_protocol::methods).
  var M = {
    SESSION_OPEN: "session/open",
    SESSION_HYDRATE: "session/hydrate",
    TURN_START: "turn/start",
    TURN_INTERRUPT: "turn/interrupt",
    TURN_STARTED: "turn/started",
    TURN_COMPLETED: "turn/completed",
    TURN_ERROR: "turn/error",
    MESSAGE_DELTA: "message/delta",
    MESSAGE_PERSISTED: "message/persisted",
    TOOL_STARTED: "tool/started",
    TOOL_PROGRESS: "tool/progress",
    TOOL_COMPLETED: "tool/completed",
    APPROVAL_REQUESTED: "approval/requested",
    APPROVAL_RESPOND: "approval/respond",
    APPROVAL_DECIDED: "approval/decided",
    APPROVAL_CANCELLED: "approval/cancelled",
    DIFF_PREVIEW_GET: "diff/preview/get",
    TASK_LIST: "task/list",
    TASK_CANCEL: "task/cancel",
    TASK_RESTART: "task/restart_from_node",
    TASK_OUTPUT_READ: "task/output/read",
    TASK_OUTPUT_DELTA: "task/output/delta",
    TASK_UPDATED: "task/updated",
    PROGRESS_UPDATED: "progress/updated",
    WARNING: "warning",
    REPLAY_LOSSY: "protocol/replay_lossy",
  };

  var CURSOR_OUT_OF_RANGE = -32110;

  // ---- module state ------------------------------------------------------

  var token = sessionStorage.getItem(TOKEN_STORAGE_KEY) || "";
  var currentSession = localStorage.getItem(SESSION_STORAGE_KEY) || "default";
  var sending = false;
  var currentAbort = null;
  var taskRefreshSeq = 0;
  var taskSnapshots = new Map();
  var connectionMode = "";

  // Live-mode reducer state. Keyed by turn_id (typed). Mirrors
  // `fixedReducer` in crates/octos-web/src/state/__tests__/lib/fixed-reducer.ts.
  var threadOrder = [];
  var threads = new Map(); // turn_id -> { turn_id, user, asst, dom: { div, body } }
  var currentTurnId = null; // most-recently-started turn (used for cancel)
  var pendingApprovals = new Map(); // approval_id -> {payload, dom}

  // Cursor returned by session/open or session/hydrate; used for replay.
  var lastCursor = null;

  // WS client + reconnect state.
  var ws = null;
  var wsClosed = false;
  var rpcPending = new Map(); // id -> {resolve, reject, timer}
  var rpcSeq = 0;
  var reconnectAttempt = 0;

  // ---- DOM refs ---------------------------------------------------------

  var messagesEl = document.getElementById("messages");
  var taskStatusEl = document.getElementById("task-status");
  var inputEl = document.getElementById("input");
  var formEl = document.getElementById("chat-form");
  var sendButton = document.getElementById("send-button");
  var cancelButton = document.getElementById("cancel-button");
  var sessionListEl = document.getElementById("session-list");
  var statusEl = document.getElementById("status-text");
  var connectionModeEl = document.getElementById("connection-mode");
  var newSessionBtn = document.getElementById("new-session");
  var authModal = document.getElementById("auth-modal");
  var authTokenEl = document.getElementById("auth-token");
  var authSubmitBtn = document.getElementById("auth-submit");

  // ---- utilities --------------------------------------------------------

  function headers() {
    var h = { "Content-Type": "application/json" };
    if (token) h.Authorization = "Bearer " + token;
    return h;
  }

  function persistCurrentSession(id) {
    currentSession = id;
    localStorage.setItem(SESSION_STORAGE_KEY, id);
  }

  function showAuth() { authModal.classList.remove("hidden"); }
  function hideAuth() { authModal.classList.add("hidden"); }

  function setConnectionMode(mode) {
    connectionMode = mode;
    if (connectionModeEl) {
      connectionModeEl.textContent = mode === LIVE_WS ? "ws" : (mode === LEGACY_REST ? "rest" : "");
      connectionModeEl.dataset.mode = mode;
    }
  }

  // Browser WebSocket API does NOT let JS set Authorization headers, so
  // tokens go through the `?token=` query param. Spec § 3.1 + the existing
  // SSE EventSource path already use this pattern. The
  // `Sec-WebSocket-Protocol: octos-bearer.<TOKEN>` subprotocol is cleaner
  // (no access-log leak) but requires a server-side change in
  // `extract_token` — not in scope for PR J.
  function buildWsUrl() {
    var base = window.location.origin
      .replace(/^http:/, "ws:")
      .replace(/^https:/, "wss:");
    var url = base + "/api/ui-protocol/ws";
    if (token) url += "?token=" + encodeURIComponent(token);
    return url;
  }

  // UUIDv7 minted client-side. The server validates and stamps it onto the
  // turn envelope; the reducer keys exclusively on this id (no sticky-map
  // fallback). Falls back to a UUIDv4-shaped string if crypto.randomUUID is
  // unavailable on this browser.
  function freshTurnId() {
    if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
      return crypto.randomUUID();
    }
    return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, function (c) {
      var r = (Math.random() * 16) | 0;
      var v = c === "x" ? r : (r & 0x3) | 0x8;
      return v.toString(16);
    });
  }

  function freshRpcId() {
    rpcSeq += 1;
    return "rpc-" + Date.now() + "-" + rpcSeq;
  }

  function humanize(value) {
    var text = String(value || "").replace(/[_-]+/g, " ").trim();
    if (!text) return "";
    return text.replace(/\b\w/g, function (ch) { return ch.toUpperCase(); });
  }

  function normalizeProgress(value) {
    if (typeof value !== "number" || !isFinite(value)) return null;
    var p = value <= 1 ? value * 100 : value;
    return Math.max(0, Math.min(100, p));
  }

  function getTaskDetail(task) {
    // The UI Protocol v1 `task/updated` notification surfaces
    // `runtime_detail` as an `Option<String>` per
    // octos-core::ui_protocol::TaskUpdatedEvent. The legacy REST shape
    // delivers it as an object. Decode either to the legacy-shape view.
    var raw = task ? task.runtime_detail : null;
    var detail;
    var rawString = null;
    if (raw && typeof raw === "object") {
      detail = raw;
    } else if (typeof raw === "string" && raw.length > 0) {
      // The wire shape is `Option<String>` — typically JSON, but the
      // server may emit a plain status message ("Writing report"). Decode
      // JSON when possible; otherwise surface the raw string as a
      // progress message so the user sees something meaningful.
      try {
        detail = JSON.parse(raw);
        if (!detail || typeof detail !== "object") {
          detail = {};
          rawString = raw;
        }
      } catch (_) {
        detail = {};
        rawString = raw;
      }
    } else {
      detail = {};
    }
    return {
      workflowKind: (task && task.workflow_kind) || detail.workflow_kind || (task && task.title) || "",
      currentPhase: (task && task.current_phase) || detail.current_phase || "",
      progressMessage: detail.progress_message || (task && task.progress_message) || rawString || "",
      progress: normalizeProgress(detail.progress != null ? detail.progress : (task && task.progress)),
      lifecycleState: (task && task.lifecycle_state) || (task && task.state) || "",
      status: (task && task.status) || "",
      runtimeState: (task && task.runtime_state) || "",
    };
  }

  function taskKey(task) {
    if (!task) return "";
    return (
      task.id ||
      task.task_id ||
      task.child_session_key ||
      task.tool_call_id ||
      task.session_key ||
      task.tool_name ||
      JSON.stringify({ started_at: task.started_at, updated_at: task.updated_at, status: task.status })
    );
  }

  function isActiveTask(task) {
    var status = String((task && task.status) || "").toLowerCase();
    var lifecycle = String((task && task.lifecycle_state) || (task && task.state) || "").toLowerCase();
    return (
      status === "spawned" || status === "running" ||
      lifecycle === "queued" || lifecycle === "running" || lifecycle === "verifying"
    );
  }

  function clearTaskIndicators() { taskStatusEl.innerHTML = ""; }

  function buildTaskIndicator(task) {
    var detail = getTaskDetail(task);
    var title = detail.workflowKind || task.tool_name || "Background task";
    var phase = detail.currentPhase || detail.lifecycleState || detail.status || "Running";
    var status = detail.lifecycleState || detail.status || detail.runtimeState || "running";
    var indicator = document.createElement("div");
    indicator.className = "session-task-indicator";
    indicator.setAttribute("data-testid", "session-task-indicator");
    indicator.dataset.taskKey = taskKey(task);
    indicator.dataset.sessionId = task.session_key || task.session_id || currentSession;
    indicator.dataset.status = String(task.status || "");
    indicator.dataset.lifecycleState = String(task.lifecycle_state || task.state || "");

    var spinner = document.createElement("div");
    spinner.className = "session-task-spinner";
    spinner.setAttribute("aria-hidden", "true");

    var content = document.createElement("div");
    content.className = "session-task-content";

    var headline = document.createElement("div");
    headline.className = "session-task-headline";

    var workflow = document.createElement("span");
    workflow.className = "session-task-workflow";
    workflow.setAttribute("data-testid", "task-workflow-kind");
    workflow.textContent = humanize(title);

    var phaseLabel = document.createElement("span");
    phaseLabel.className = "session-task-phase";
    phaseLabel.setAttribute("data-testid", "task-current-phase");
    phaseLabel.textContent = humanize(phase);

    var statusLabel = document.createElement("span");
    statusLabel.className = "session-task-status";
    statusLabel.setAttribute("data-testid", "task-status-label");
    statusLabel.textContent = humanize(status);

    headline.appendChild(workflow);
    headline.appendChild(document.createTextNode("·"));
    headline.appendChild(phaseLabel);
    headline.appendChild(document.createTextNode("·"));
    headline.appendChild(statusLabel);

    if (detail.progress !== null) {
      var progressLabel = document.createElement("span");
      progressLabel.className = "session-task-status";
      progressLabel.setAttribute("data-testid", "task-progress-value");
      progressLabel.textContent = Math.round(detail.progress) + "%";
      headline.appendChild(document.createTextNode("·"));
      headline.appendChild(progressLabel);
    }

    content.appendChild(headline);

    var message = detail.progressMessage || (phase ? humanize(phase) + "..." : statusLabel.textContent || "Working...");
    var messageEl = document.createElement("div");
    messageEl.className = "session-task-message";
    messageEl.setAttribute("data-testid", "task-progress-message");
    messageEl.textContent = message;
    content.appendChild(messageEl);

    if (detail.progress !== null) {
      var progressWrap = document.createElement("div");
      progressWrap.className = "session-task-progress";
      progressWrap.setAttribute("data-testid", "task-progress");
      var progressBar = document.createElement("div");
      progressBar.className = "session-task-progress-bar";
      progressBar.style.setProperty("--progress", detail.progress + "%");
      progressBar.setAttribute("aria-hidden", "true");
      progressWrap.appendChild(progressBar);
      content.appendChild(progressWrap);
    }

    indicator.appendChild(spinner);
    indicator.appendChild(content);
    return indicator;
  }

  function renderTaskIndicators(sessionId) {
    if (sessionId !== currentSession) return;

    var activeTasks = [];
    var seen = new Set();
    taskSnapshots.forEach(function (entry, key) {
      if (entry.sessionId !== sessionId) return;
      if (!isActiveTask(entry.task)) return;
      if (seen.has(key)) return;
      seen.add(key);
      activeTasks.push(entry.task);
    });

    activeTasks.sort(function (a, b) {
      var aTime = new Date((a && (a.updated_at || a.started_at)) || 0).getTime();
      var bTime = new Date((b && (b.updated_at || b.started_at)) || 0).getTime();
      return aTime - bTime;
    });

    taskStatusEl.innerHTML = "";
    activeTasks.forEach(function (task) {
      taskStatusEl.appendChild(buildTaskIndicator(task));
    });
  }

  function upsertTaskSnapshot(sessionId, task) {
    var key = taskKey(task);
    if (!key) return;
    taskSnapshots.set(key, { sessionId: sessionId, task: task });
  }

  function syncTasks(sessionId, tasks) {
    var seen = new Set();
    (tasks || []).forEach(function (task) {
      var key = taskKey(task);
      if (!key) return;
      seen.add(key);
      taskSnapshots.set(key, { sessionId: sessionId, task: task });
    });

    taskSnapshots.forEach(function (entry, key) {
      if (entry.sessionId === sessionId && !seen.has(key)) {
        taskSnapshots.delete(key);
      }
    });

    renderTaskIndicators(sessionId);
  }

  // ---- REST helpers (the 8 actions that stay REST per spec § 11) --------

  async function fetchJson(url, opts) {
    var resp = await fetch(url, opts);
    if (resp.status === 401) { showAuth(); throw new Error("unauthorized"); }
    if (!resp.ok) throw new Error("HTTP " + resp.status);
    return resp.json();
  }

  async function loadSessions() {
    try {
      var data = await fetchJson("/api/sessions", { headers: headers() });
      if (!Array.isArray(data)) return [];
      sessionListEl.innerHTML = "";
      data.forEach(function (s) {
        var li = document.createElement("li");
        li.dataset.id = s.id;
        li.dataset.sessionId = s.id;
        li.dataset.active = s.id === currentSession ? "true" : "false";
        if (s.id === currentSession) li.className = "active";

        var title = document.createElement("button");
        title.type = "button";
        title.className = "session-switch-button";
        title.setAttribute("data-testid", "session-switch-button");
        title.textContent = s.id + " (" + s.message_count + ")";
        title.addEventListener("click", function () { selectSession(s.id); });

        var del = document.createElement("button");
        del.type = "button";
        del.className = "session-delete";
        del.setAttribute("data-testid", "session-delete-button");
        del.title = "Delete session";
        del.textContent = "x";
        del.addEventListener("click", function (e) {
          e.stopPropagation();
          deleteSession(s.id);
        });

        li.appendChild(title);
        li.appendChild(del);
        sessionListEl.appendChild(li);
      });
      return data;
    } catch (error) {
      return [];
    }
  }

  function deleteSession(id) {
    // Stay REST per spec § 11 — infrequent admin op.
    if (!id || !window.confirm('Delete session "' + id + '"?')) return;
    fetch("/api/sessions/" + encodeURIComponent(id), { method: "DELETE", headers: headers() })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return; }
        if (id === currentSession) {
          persistCurrentSession("default");
          messagesEl.innerHTML = "";
          clearLiveState();
          clearTaskIndicators();
        }
        loadSessions();
      })
      .catch(function () {});
  }

  // Used in LEGACY-REST mode AND as fallback in LIVE-WS mode if the server
  // does not advertise `state.session_hydrate.v1` capability.
  async function loadHistoryRest(id) {
    messagesEl.innerHTML = "";
    clearLiveState();
    try {
      var msgs = await fetchJson(
        "/api/sessions/" + encodeURIComponent(id) + "/messages?limit=100",
        { headers: headers() },
      );
      if (!Array.isArray(msgs)) return;
      msgs.forEach(function (m) {
        if (m.media && m.media.length > 0) {
          m.media.forEach(function (path) {
            var name = path.split("/").pop() || "file";
            appendFileMessage(name, path, "");
          });
        } else {
          appendMessageDom(m.role.toLowerCase(), m.content, /* turn_id */ null);
        }
      });
    } catch (_) {}
  }

  // Stays REST per spec § 11 (low-frequency health snapshot).
  function pollStatus() {
    fetch("/api/status", { headers: headers() })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return null; }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        var uptime = Math.floor(data.uptime_secs / 60);
        statusEl.textContent =
          data.model + " | " + data.provider + " | up " + uptime + "m | v" + data.version;
      })
      .catch(function () { statusEl.textContent = "Disconnected"; });
  }

  // ---- DOM rendering ----------------------------------------------------

  function appendMessageDom(role, content, turn_id) {
    var div = document.createElement("div");
    div.className = "message " + role;
    div.setAttribute("data-testid", role + "-message");
    if (turn_id) div.setAttribute("data-thread-id", turn_id);

    var roleLabel = document.createElement("div");
    roleLabel.className = "role";
    roleLabel.textContent = role;
    div.appendChild(roleLabel);

    var body = document.createElement("div");
    body.textContent = content;
    div.appendChild(body);

    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return { div: div, body: body };
  }

  function appendFileMessage(filename, path, caption, turn_id) {
    var div = document.createElement("div");
    div.className = "message assistant";
    div.setAttribute("data-testid", "assistant-message");
    if (turn_id) div.setAttribute("data-thread-id", turn_id);

    var roleLabel = document.createElement("div");
    roleLabel.className = "role";
    roleLabel.textContent = "assistant";
    div.appendChild(roleLabel);

    var body = document.createElement("div");
    var fileUrl = "/api/files?path=" + encodeURIComponent(path);
    var ext = (filename || "").split(".").pop().toLowerCase();
    var attachment = document.createElement("div");
    attachment.className = "audio-attachment";
    attachment.setAttribute("data-testid", "audio-attachment");
    attachment.dataset.filename = filename || "";
    attachment.dataset.filePath = path || "";

    if (ext === "mp3" || ext === "wav" || ext === "ogg" || ext === "m4a") {
      var audio = document.createElement("audio");
      audio.controls = true;
      audio.src = fileUrl;
      attachment.appendChild(audio);
      if (caption) {
        var cap = document.createElement("div");
        cap.textContent = caption;
        attachment.appendChild(cap);
      }
    } else {
      var a = document.createElement("a");
      a.href = fileUrl;
      a.download = filename;
      a.textContent = filename || "Download file";
      attachment.appendChild(a);
    }

    body.appendChild(attachment);
    div.appendChild(body);
    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function clearLiveState() {
    threadOrder = [];
    threads.clear();
    currentTurnId = null;
    pendingApprovals.forEach(function (p) {
      if (p.dom && p.dom.parentNode) p.dom.parentNode.removeChild(p.dom);
    });
    pendingApprovals.clear();
  }

  // ---- Reducer (mirrors fixedReducer in crates/octos-web) ---------------
  //
  // KEY INVARIANT: every turn-scoped event MUST carry `turn_id`. Events
  // without `turn_id` are dropped (no sticky fallback). This is the
  // structural fix that closes the M8.10 thread-binding bug class.

  function ensureThread(turn_id) {
    if (threads.has(turn_id)) return threads.get(turn_id);
    var thread = { turn_id: turn_id, user: "", asst: "", dom: null };
    threads.set(turn_id, thread);
    threadOrder.push(turn_id);
    return thread;
  }

  function ensureUserBubble(turn_id, text) {
    var t = ensureThread(turn_id);
    if (!t.userDom) {
      t.userDom = appendMessageDom("user", text, turn_id);
      t.user = text;
    } else if (text && t.user !== text) {
      t.user = text;
      t.userDom.body.textContent = text;
    }
    return t;
  }

  function ensureAssistantBubble(turn_id) {
    var t = ensureThread(turn_id);
    if (!t.dom) {
      t.dom = appendMessageDom("assistant", "", turn_id);
      t.dom.div.classList.add("streaming");
    }
    return t;
  }

  function reduceMessageDelta(p) {
    if (!p || !p.turn_id) return;
    if (!threads.has(p.turn_id)) return; // strict: no orphans
    var t = ensureAssistantBubble(p.turn_id);
    t.asst += String(p.text || "");
    t.dom.body.textContent = t.asst;
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function reduceTurnStarted(p) {
    if (!p || !p.turn_id) return;
    ensureThread(p.turn_id);
    currentTurnId = p.turn_id;
  }

  function reduceTurnCompleted(p) {
    if (!p || !p.turn_id) return;
    var t = threads.get(p.turn_id);
    if (t && t.dom) t.dom.div.classList.remove("streaming");
    if (currentTurnId === p.turn_id) {
      currentTurnId = null;
      finishStreaming();
    }
    if (p.cursor) lastCursor = p.cursor;
  }

  function reduceTurnError(p) {
    if (!p || !p.turn_id) return;
    var t = ensureThread(p.turn_id);
    var bubble = t.dom || appendMessageDom("assistant", "", p.turn_id);
    bubble.body.textContent = "Error: " + (p.message || p.code || "unknown");
    bubble.div.classList.add("error");
    bubble.div.classList.remove("streaming");
    if (currentTurnId === p.turn_id) {
      currentTurnId = null;
      finishStreaming();
    }
  }

  function reduceMessagePersisted(p) {
    // UPCR-2026-012: durable confirmation of a row. Used to advance
    // `lastCursor` so reconnect replay starts from the right point. The
    // assistant bubble is already finalized by `turn/completed` so we
    // don't overwrite content here.
    if (p && p.cursor) lastCursor = p.cursor;
  }

  // Tool-call binding mirrors fixedReducer in
  // crates/octos-web/src/state/__tests__/lib/fixed-reducer.ts: every tool
  // event MUST carry turn_id; the call lands on that turn's bubble. There
  // is NO sticky-map fallback — that is the structural fix.
  function reduceToolStarted(p) {
    if (!p || !p.turn_id || !p.tool_call_id) return;
    var t = ensureThread(p.turn_id);
    t.tool_calls = (t.tool_calls || []).concat([
      { tool_call_id: p.tool_call_id, tool_name: p.tool_name },
    ]);
  }

  function reduceToolCompleted(p) {
    if (!p || !p.turn_id || !p.tool_call_id) return;
    var t = ensureThread(p.turn_id);
    var existing = (t.tool_calls || []).find(function (x) { return x.tool_call_id === p.tool_call_id; });
    if (existing) {
      existing.success = p.success;
      existing.output_preview = p.output_preview;
    } else {
      t.tool_calls = (t.tool_calls || []).concat([
        {
          tool_call_id: p.tool_call_id,
          tool_name: p.tool_name,
          success: p.success,
          output_preview: p.output_preview,
        },
      ]);
    }
  }

  function reduceWarning(p) {
    if (!p) return;
    console.warn("[octos] warning:", p.code, p.message);
  }

  function reduceReplayLossy(p) {
    // Server signals it dropped notifications. Re-hydrate authoritatively.
    console.warn("[octos] replay_lossy — rehydrating", p);
    if (currentSession) {
      hydrateOrFallback(currentSession).catch(function () {});
    }
  }

  // ---- WS client --------------------------------------------------------

  function rpcReject(id, err) {
    var p = rpcPending.get(id);
    if (!p) return;
    rpcPending.delete(id);
    clearTimeout(p.timer);
    p.reject(err);
  }

  function wsRequest(method, params, timeoutMs) {
    return new Promise(function (resolve, reject) {
      if (!ws || wsClosed || ws.readyState !== WebSocket.OPEN) {
        return reject(new Error("ws: not open"));
      }
      var id = freshRpcId();
      var tmo = timeoutMs == null ? 30000 : timeoutMs;
      var timer = setTimeout(function () {
        rpcPending.delete(id);
        reject(new Error("ws: timeout waiting for " + method));
      }, tmo);
      rpcPending.set(id, { resolve: resolve, reject: reject, timer: timer });
      try {
        ws.send(JSON.stringify({ jsonrpc: "2.0", id: id, method: method, params: params }));
      } catch (err) {
        rpcReject(id, err);
      }
    });
  }

  function handleWsFrame(text) {
    var msg;
    try { msg = JSON.parse(text); } catch (_) { return; }
    if (!msg || typeof msg !== "object") return;

    if (msg.id != null && (Object.prototype.hasOwnProperty.call(msg, "result") || msg.error)) {
      var p = rpcPending.get(String(msg.id));
      if (!p) return;
      rpcPending.delete(String(msg.id));
      clearTimeout(p.timer);
      if (msg.error) {
        var err = new Error("rpc[" + msg.error.code + "]: " + msg.error.message);
        err.code = msg.error.code;
        err.data = msg.error.data;
        return p.reject(err);
      }
      return p.resolve(msg.result);
    }

    if (msg.method) {
      handleNotification(msg.method, msg.params || {});
    }
  }

  function handleNotification(method, params) {
    // Cursor monotonicity: advance lastCursor whenever a notification
    // carries one. Mirrors M9WsClient.handleMessage.
    if (params && params.cursor && typeof params.cursor.seq === "number") {
      lastCursor = params.cursor;
    }

    switch (method) {
      case M.SESSION_OPEN:
        // server-pushed mirror of session/open's result; cursor handled above.
        return;
      case M.TURN_STARTED:
        return reduceTurnStarted(params);
      case M.MESSAGE_DELTA:
        return reduceMessageDelta(params);
      case M.TURN_COMPLETED:
        return reduceTurnCompleted(params);
      case M.TURN_ERROR:
        return reduceTurnError(params);
      case M.MESSAGE_PERSISTED:
        return reduceMessagePersisted(params);
      case M.TOOL_STARTED:
        return reduceToolStarted(params);
      case M.TOOL_PROGRESS:
        // No render-state mutation needed for binding correctness; the
        // production UI may render progress text but the binding contract
        // is fully expressed via tool_started + tool_completed (matches
        // fixed-reducer.ts).
        return;
      case M.TOOL_COMPLETED:
        return reduceToolCompleted(params);
      case M.TASK_UPDATED:
        if (!params || !params.session_id) return;
        upsertTaskSnapshot(params.session_id, params);
        if (!isActiveTask(params)) {
          // On terminal task state, deliver any spawn-only output files.
          // The legacy REST path achieved this by polling
          // `/api/sessions/:id/messages` for new media[]; the WS path uses
          // task/output/read which directly returns `output_files`.
          deliverTaskOutputFiles(params);
          var key = taskKey(params);
          if (key) taskSnapshots.delete(key);
        }
        renderTaskIndicators(params.session_id);
        return;
      case M.TASK_OUTPUT_DELTA:
        // Streaming task stdout chunks; surface on the indicator's progress
        // message so users see live output for long-running spawn-only
        // tools without us having to allocate a fresh DOM bubble per chunk.
        if (params && params.task_id) {
          var prev = taskSnapshots.get(params.task_id);
          // If we receive output for a task we have not seen `task/updated`
          // for yet, synthesize a running snapshot so isActiveTask doesn't
          // immediately filter it out. (Server should always emit
          // `task/updated` first, but guard defensively.)
          var t2 = (prev && prev.task) ? prev.task : {
            task_id: params.task_id,
            session_id: params.session_id,
            state: "running",
          };
          t2.progress_message = String(params.text || "").slice(-512);
          upsertTaskSnapshot(params.session_id || currentSession, t2);
          renderTaskIndicators(params.session_id || currentSession);
        }
        return;
      case M.PROGRESS_UPDATED:
        // task_id-keyed progress; treat as a task snapshot update.
        if (params && params.session_id && params.task_id) {
          var existing = taskSnapshots.get(params.task_id);
          var task = existing && existing.task ? existing.task : { task_id: params.task_id };
          task.progress = params.progress;
          task.progress_message = params.message || task.progress_message;
          upsertTaskSnapshot(params.session_id, task);
          renderTaskIndicators(params.session_id);
        }
        return;
      case M.APPROVAL_REQUESTED:
        return showApprovalPrompt(params);
      case M.APPROVAL_DECIDED:
      case M.APPROVAL_CANCELLED:
        return clearApprovalPrompt(params && params.approval_id);
      case M.WARNING:
        return reduceWarning(params);
      case M.REPLAY_LOSSY:
        return reduceReplayLossy(params);
      default:
        // Unknown notifications are ignored per spec §4 (forward-compat).
        return;
    }
  }

  function attemptWsHandshake() {
    return new Promise(function (resolve, reject) {
      var url = buildWsUrl();
      var sock;
      try {
        sock = new WebSocket(url);
      } catch (err) {
        return reject(err);
      }
      var settled = false;
      var timer = setTimeout(function () {
        if (settled) return;
        settled = true;
        try { sock.close(); } catch (_) {}
        reject(new Error("ws: handshake timeout"));
      }, WS_HANDSHAKE_TIMEOUT_MS);

      sock.addEventListener("open", function () {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        ws = sock;
        wsClosed = false;
        sock.addEventListener("message", function (ev) { handleWsFrame(String(ev.data)); });
        sock.addEventListener("close", function () { onWsClose(); });
        sock.addEventListener("error", function () { /* surfaced via close */ });
        resolve();
      });
      sock.addEventListener("error", function (ev) {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        reject(ev);
      });
    });
  }

  function onWsClose() {
    if (wsClosed) return;
    wsClosed = true;
    ws = null;
    rpcPending.forEach(function (p) {
      clearTimeout(p.timer);
      p.reject(new Error("ws: socket closed"));
    });
    rpcPending.clear();
    finishStreaming();

    if (connectionMode !== LIVE_WS) return;

    if (reconnectAttempt < WS_RECONNECT_BACKOFF_MS.length) {
      var delay = WS_RECONNECT_BACKOFF_MS[reconnectAttempt++];
      setTimeout(function () {
        reconnectLive().catch(function () {});
      }, delay);
    } else {
      statusEl.textContent = "Disconnected — refresh to reconnect";
    }
  }

  async function reconnectLive() {
    try {
      await attemptWsHandshake();
      reconnectAttempt = 0;
      // Reuse the persisted cursor so the server replays durable events
      // we missed during the gap.
      var openParams = { session_id: currentSession };
      if (lastCursor) openParams.after = lastCursor;
      try {
        await wsRequest(M.SESSION_OPEN, openParams);
      } catch (err) {
        if (err && err.code === CURSOR_OUT_OF_RANGE) {
          // Server can no longer replay from our cursor — rehydrate.
          lastCursor = null;
          await hydrateOrFallback(currentSession);
        } else {
          throw err;
        }
      }
    } catch (err) {
      // If reconnect fails repeatedly, the WS-close handler will count
      // attempts and give up.
      if (ws == null) onWsClose();
    }
  }

  // ---- session/hydrate (UPCR-2026-009) ----------------------------------

  async function hydrateOrFallback(sessionId) {
    messagesEl.innerHTML = "";
    clearLiveState();
    try {
      var result = await wsRequest(M.SESSION_HYDRATE, { session_id: sessionId });
      if (result && result.cursor) lastCursor = result.cursor;

      var messages = (result && result.messages) || [];
      // Rebuild the thread DOM from the authoritative server snapshot.
      messages.forEach(function (m) {
        var role = String(m.role || "").toLowerCase();
        var tid = m.turn_id || null;
        if (role === "user" && tid) {
          ensureUserBubble(tid, m.content || "");
          return;
        }
        if (role === "assistant" && tid) {
          var t = ensureAssistantBubble(tid);
          t.asst = m.content || "";
          t.dom.body.textContent = t.asst;
          t.dom.div.classList.remove("streaming");
          return;
        }
        // Fallback for rows without turn_id (legacy rows etc.) — render
        // without binding so the user still sees their history.
        appendMessageDom(role, m.content || "", null);
      });

      // Replay pending approvals so the modal pops up.
      var pa = (result && result.pending_approvals) || [];
      pa.forEach(showApprovalPrompt);
    } catch (err) {
      // If hydrate isn't supported (server pre-UPCR-2026-009) fall back
      // to the legacy REST messages endpoint.
      console.warn("[octos] session/hydrate failed; falling back to REST", err);
      await loadHistoryRest(sessionId);
    }
  }

  // ---- spawn-only file delivery (task/output/read) ---------------------
  //
  // The legacy SSE path watched for `done.has_bg_tasks=true` and then
  // polled `/api/sessions/:id/messages` for new `media[]` entries. The
  // UI Protocol v1 path replaces that with: on `task/updated` with
  // terminal state, call `task/output/read` which returns the produced
  // file paths in `output_files`. File bytes themselves still come over
  // REST `/api/files?path=…` per spec § 11.

  // Per-session dedupe: keying on (session_id, path) prevents a file
  // delivered into session A from suppressing the same path in a later
  // session B (e.g. same temp dir reused). The legacy REST poll dedupe at
  // app.js:612-658 (pre-PR-J) was per-session-by-construction because the
  // poll was scoped to one session id; the WS path needs explicit scoping.
  var deliveredFiles = new Set();
  function deliveredKey(sid, path) { return sid + " " + path; }

  function deliverTaskOutputFiles(taskEvent) {
    if (!taskEvent || !taskEvent.session_id || !taskEvent.task_id) return;
    var sid = taskEvent.session_id;
    var tid = taskEvent.task_id;
    var originating_turn_id = taskEvent.originating_turn_id || null;
    // Pre-flight: only fetch if the user is currently viewing this session.
    if (sid !== currentSession) return;
    wsRequest(M.TASK_OUTPUT_READ, { session_id: sid, task_id: tid }).then(function (res) {
      // The user may have switched sessions while the RPC was in flight.
      // Re-validate before mutating DOM — otherwise we'd render files into
      // the wrong session's message list.
      if (sid !== currentSession) return;
      if (!res) return;
      var files = res.output_files || [];
      var turn_id = res.originating_turn_id || originating_turn_id;
      files.forEach(function (path) {
        if (!path) return;
        var k = deliveredKey(sid, path);
        if (deliveredFiles.has(k)) return;
        deliveredFiles.add(k);
        var name = String(path).split("/").pop() || "file";
        appendFileMessage(name, path, "", turn_id);
      });
    }).catch(function (err) {
      // task/output/read may not be supported (or task may not exist
      // anymore); silent — we're delivering opportunistically.
      console.debug("[octos] task/output/read failed", err);
    });
  }

  // ---- approvals --------------------------------------------------------

  function showApprovalPrompt(params) {
    if (!params || !params.approval_id) return;
    var existing = pendingApprovals.get(params.approval_id);
    if (existing) return;

    var div = document.createElement("div");
    div.className = "message system";
    div.setAttribute("data-testid", "approval-request");
    div.dataset.approvalId = params.approval_id;
    if (params.turn_id) div.setAttribute("data-thread-id", params.turn_id);

    var header = document.createElement("div");
    header.className = "role";
    header.textContent = "approval";
    div.appendChild(header);

    var body = document.createElement("div");
    var kind = (params.typed_details && params.typed_details.kind) || params.approval_kind || "approval";
    var title = document.createElement("div");
    title.textContent = "Approval required: " + humanize(kind);
    body.appendChild(title);

    if (params.typed_details && params.typed_details.command && params.typed_details.command.command_line) {
      var pre = document.createElement("pre");
      pre.textContent = params.typed_details.command.command_line;
      body.appendChild(pre);
    }

    if (params.typed_details && params.typed_details.diff && params.typed_details.diff.preview_id) {
      // Fetch + render diff preview lazily via diff/preview/get RPC.
      wsRequest(M.DIFF_PREVIEW_GET, {
        session_id: params.session_id,
        preview_id: params.typed_details.diff.preview_id,
      }).then(function (res) {
        var summary = document.createElement("pre");
        summary.setAttribute("data-testid", "diff-preview");
        summary.textContent = JSON.stringify(res && res.preview ? res.preview : res, null, 2);
        body.appendChild(summary);
      }).catch(function () {});
    }

    var actions = document.createElement("div");
    actions.className = "approval-actions";

    function respond(decision) {
      wsRequest(M.APPROVAL_RESPOND, {
        session_id: params.session_id,
        approval_id: params.approval_id,
        decision: decision,
      }).catch(function (err) {
        console.warn("[octos] approval/respond failed", err);
      }).then(function () {
        clearApprovalPrompt(params.approval_id);
      });
    }

    var approveBtn = document.createElement("button");
    approveBtn.type = "button";
    approveBtn.setAttribute("data-testid", "approval-approve");
    approveBtn.textContent = "Approve";
    approveBtn.addEventListener("click", function () { respond("approve"); });

    var denyBtn = document.createElement("button");
    denyBtn.type = "button";
    denyBtn.setAttribute("data-testid", "approval-deny");
    denyBtn.textContent = "Deny";
    denyBtn.addEventListener("click", function () { respond("deny"); });

    actions.appendChild(approveBtn);
    actions.appendChild(denyBtn);
    body.appendChild(actions);
    div.appendChild(body);
    messagesEl.appendChild(div);
    pendingApprovals.set(params.approval_id, { payload: params, dom: div });
  }

  function clearApprovalPrompt(approvalId) {
    if (!approvalId) return;
    var entry = pendingApprovals.get(approvalId);
    if (!entry) return;
    pendingApprovals.delete(approvalId);
    if (entry.dom && entry.dom.parentNode) entry.dom.parentNode.removeChild(entry.dom);
  }

  // ---- LIVE-WS send turn ------------------------------------------------

  async function sendTurnLive(text) {
    var turn_id = freshTurnId();
    // Optimistically render the user bubble bound to the typed turn_id.
    ensureUserBubble(turn_id, text);
    ensureAssistantBubble(turn_id);
    currentTurnId = turn_id;
    sending = true;
    sendButton.disabled = true;
    cancelButton.classList.remove("hidden");
    try {
      await wsRequest(M.TURN_START, {
        session_id: currentSession,
        turn_id: turn_id,
        input: [{ kind: "text", text: text }],
      });
      // The notification stream finishes the turn (turn/completed).
      // Refresh the sidebar so message_count tracks.
      loadSessions();
    } catch (err) {
      reduceTurnError({
        session_id: currentSession,
        turn_id: turn_id,
        code: "send_failed",
        message: (err && err.message) || "send failed",
      });
    }
  }

  async function cancelTurnLive() {
    if (!currentTurnId) {
      finishStreaming();
      return;
    }
    var tid = currentTurnId;
    try {
      await wsRequest(M.TURN_INTERRUPT, { session_id: currentSession, turn_id: tid });
    } catch (err) {
      console.warn("[octos] turn/interrupt failed", err);
    }
    finishStreaming();
  }

  // ---- LEGACY-REST send turn (fallback) --------------------------------
  //
  // Identical to the pre-PR-J path. Engages only when WS handshake fails
  // at boot. Kept verbatim so existing deployments behind WS-hostile
  // proxies see no behaviour change.

  function parseSseChunk(buffer, text, handler) {
    buffer += text;
    var lines = buffer.split("\n");
    buffer = lines.pop();
    lines.forEach(function (line) {
      if (line.indexOf("data:") !== 0) return;
      var json = line.slice(5).trim();
      if (!json) return;
      try { handler(JSON.parse(json)); } catch (_) {}
    });
    return buffer;
  }

  function sendTurnLegacy(text) {
    sending = true;
    sendButton.disabled = true;
    cancelButton.classList.remove("hidden");

    appendMessageDom("user", text, null);
    var assistantBubble = appendMessageDom("assistant", "", null);
    assistantBubble.div.classList.add("streaming");
    var bodyEl = assistantBubble.body;
    var accumulated = "";
    var sid = currentSession;
    var finished = false;
    currentAbort = new AbortController();

    function finish() {
      if (finished) return;
      finished = true;
      assistantBubble.div.classList.remove("streaming");
      finishStreaming();
    }

    fetch("/api/chat", {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ message: text, session_id: currentSession }),
      signal: currentAbort.signal,
    })
      .then(function (r) {
        if (r.status === 401) { showAuth(); finish(); return null; }
        if (!r.body) { finish(); return null; }
        var reader = r.body.getReader();
        var decoder = new TextDecoder();
        var buf = "";

        function read() {
          reader.read().then(function (result) {
            if (result.done) { finish(); return; }
            buf = parseSseChunk(buf, decoder.decode(result.value, { stream: true }), function (data) {
              if (data.type === "keepalive") return;
              if (data.type === "task_status" && data.task) {
                upsertTaskSnapshot(sid, data.task);
                renderTaskIndicators(sid);
                return;
              }
              if ((data.type === "token" || data.type === "delta") && data.text) {
                accumulated += data.text;
                bodyEl.textContent = accumulated;
                messagesEl.scrollTop = messagesEl.scrollHeight;
              } else if (data.type === "replace" && data.text) {
                accumulated = data.text;
                bodyEl.textContent = accumulated;
                messagesEl.scrollTop = messagesEl.scrollHeight;
              } else if (data.type === "done") {
                if (accumulated) bodyEl.textContent = accumulated;
                loadSessions();
                refreshTaskStatusRest(sid);
                if (data.has_bg_tasks) pollForBgFiles(sid);
                finish();
              } else if (data.type === "file") {
                appendFileMessage(data.filename, data.path, data.caption, null);
              }
            });
            read();
          }).catch(function (err) {
            if (err && err.name === "AbortError") { finish(); return; }
            bodyEl.textContent = "Error: " + err.message;
            finish();
          });
        }

        read();
        return null;
      })
      .catch(function (err) {
        if (err && err.name === "AbortError") return;
        bodyEl.textContent = "Error: " + err.message;
        finish();
      });
  }

  async function refreshTaskStatusRest(sessionId) {
    var requestSeq = ++taskRefreshSeq;
    try {
      var tasks = await fetchJson(
        "/api/sessions/" + encodeURIComponent(sessionId) + "/tasks",
        { headers: headers() },
      );
      if (requestSeq !== taskRefreshSeq || sessionId !== currentSession) return;
      if (!Array.isArray(tasks)) { clearTaskIndicators(); return; }
      syncTasks(sessionId, tasks);
    } catch (_) {
      if (requestSeq === taskRefreshSeq && sessionId === currentSession) {
        clearTaskIndicators();
      }
    }
  }

  function pollForBgFiles(sessionId) {
    var startTime = new Date().toISOString();
    var attempts = 0;
    var maxAttempts = 150;
    var delivered = {};

    function poll() {
      if (attempts++ >= maxAttempts) return;
      fetch("/api/sessions/" + encodeURIComponent(sessionId) + "/messages?limit=100", {
        headers: headers(),
      })
        .then(function (r) { return r.ok ? r.json() : null; })
        .then(function (msgs) {
          if (!msgs) { setTimeout(poll, 2000); return; }
          var done = false;
          msgs.forEach(function (m) {
            if (m.timestamp > startTime && m.media && m.media.length > 0) {
              m.media.forEach(function (path) {
                if (!delivered[path]) {
                  delivered[path] = true;
                  var name = path.split("/").pop() || "file";
                  appendFileMessage(name, path, "", null);
                }
              });
            }
            if (m.timestamp > startTime && m.content &&
                (m.content.charAt(0) === "✓" || m.content.charAt(0) === "✗")) {
              done = true;
            }
          });
          if (!done) setTimeout(poll, 2000);
        })
        .catch(function () { setTimeout(poll, 2000); });
    }

    setTimeout(poll, 2000);
  }

  function finishStreaming() {
    sending = false;
    currentAbort = null;
    sendButton.disabled = false;
    cancelButton.classList.add("hidden");
  }

  // ---- session selection / new session ---------------------------------

  async function selectSession(id) {
    persistCurrentSession(id);
    loadSessions();
    if (connectionMode === LIVE_WS) {
      try {
        await wsRequest(M.SESSION_OPEN, { session_id: id });
      } catch (_) {}
      await hydrateOrFallback(id);
    } else {
      await loadHistoryRest(id);
      await refreshTaskStatusRest(id);
    }
  }

  // ---- form handlers ---------------------------------------------------

  cancelButton.addEventListener("click", function () {
    if (connectionMode === LIVE_WS && currentTurnId) {
      cancelTurnLive();
    } else if (currentAbort) {
      currentAbort.abort();
      finishStreaming();
    } else {
      finishStreaming();
    }
  });

  authSubmitBtn.addEventListener("click", function () {
    token = authTokenEl.value.trim();
    sessionStorage.setItem(TOKEN_STORAGE_KEY, token);
    hideAuth();
    // Re-bootstrap so we pick up the new token for the WS handshake.
    bootstrap().catch(function () {});
  });

  newSessionBtn.addEventListener("click", function () {
    var id = "s_" + Date.now();
    persistCurrentSession(id);
    messagesEl.innerHTML = "";
    clearLiveState();
    clearTaskIndicators();
    loadSessions();
    if (connectionMode === LIVE_WS) {
      wsRequest(M.SESSION_OPEN, { session_id: id }).catch(function () {});
    }
  });

  formEl.addEventListener("submit", function (e) {
    e.preventDefault();
    var text = inputEl.value.trim();
    if (!text || sending) return;
    inputEl.value = "";

    if (connectionMode === LIVE_WS) {
      sendTurnLive(text).catch(function (err) {
        console.error("[octos] sendTurnLive failed", err);
        finishStreaming();
      });
    } else {
      sendTurnLegacy(text);
    }
  });

  inputEl.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      formEl.dispatchEvent(new Event("submit"));
    }
  });

  // ---- bootstrap --------------------------------------------------------

  async function bootstrap() {
    setConnectionMode("");
    await loadSessions();
    try {
      await attemptWsHandshake();
      // Open the session over WS first (returns capabilities + cursor).
      var openResult;
      try {
        openResult = await wsRequest(M.SESSION_OPEN, { session_id: currentSession });
      } catch (err) {
        // Treat session/open failure as a hard fallback, since the rest of
        // the LIVE-WS path depends on it.
        throw err;
      }
      if (openResult && openResult.opened && openResult.opened.cursor) {
        lastCursor = openResult.opened.cursor;
      }
      setConnectionMode(LIVE_WS);
      await hydrateOrFallback(currentSession);
    } catch (err) {
      console.warn("[octos] WS unavailable; using legacy REST/SSE", err);
      setConnectionMode(LEGACY_REST);
      try { if (ws) ws.close(); } catch (_) {}
      ws = null; wsClosed = true;
      await loadHistoryRest(currentSession);
      await refreshTaskStatusRest(currentSession);
    }
  }

  // Boot.
  bootstrap();
  pollStatus();
  setInterval(pollStatus, 30000);

  // In LEGACY-REST mode we still poll for task indicators (live-WS gets
  // them via task/updated notifications). The poll is a no-op in LIVE-WS.
  setInterval(function () {
    if (connectionMode === LEGACY_REST && currentSession) {
      refreshTaskStatusRest(currentSession);
    }
  }, 2500);
})();
