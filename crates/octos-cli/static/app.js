(function () {
  "use strict";

  var TOKEN_STORAGE_KEY = "octos_token";
  var SESSION_STORAGE_KEY = "octos_current_session";
  var token = sessionStorage.getItem(TOKEN_STORAGE_KEY) || "";
  var currentSession = localStorage.getItem(SESSION_STORAGE_KEY) || "default";
  var sending = false;
  var currentAbort = null;
  var taskRefreshSeq = 0;
  var taskSnapshots = new Map();

  var messagesEl = document.getElementById("messages");
  var taskStatusEl = document.getElementById("task-status");
  var inputEl = document.getElementById("input");
  var formEl = document.getElementById("chat-form");
  var sendButton = document.getElementById("send-button");
  var cancelButton = document.getElementById("cancel-button");
  var sessionListEl = document.getElementById("session-list");
  var statusEl = document.getElementById("status-text");
  var newSessionBtn = document.getElementById("new-session");
  var authModal = document.getElementById("auth-modal");
  var authTokenEl = document.getElementById("auth-token");
  var authSubmitBtn = document.getElementById("auth-submit");

  function headers() {
    var h = { "Content-Type": "application/json" };
    if (token) h.Authorization = "Bearer " + token;
    return h;
  }

  function persistCurrentSession(id) {
    currentSession = id;
    localStorage.setItem(SESSION_STORAGE_KEY, id);
  }

  function showAuth() {
    authModal.classList.remove("hidden");
  }

  function hideAuth() {
    authModal.classList.add("hidden");
  }

  function humanize(value) {
    var text = String(value || "").replace(/[_-]+/g, " ").trim();
    if (!text) return "";
    return text.replace(/\b\w/g, function (ch) {
      return ch.toUpperCase();
    });
  }

  function normalizeProgress(value) {
    if (typeof value !== "number" || !isFinite(value)) return null;
    var progress = value <= 1 ? value * 100 : value;
    return Math.max(0, Math.min(100, progress));
  }

  function getTaskDetail(task) {
    var detail =
      task && task.runtime_detail && typeof task.runtime_detail === "object"
        ? task.runtime_detail
        : {};
    return {
      workflowKind: task?.workflow_kind || detail.workflow_kind || "",
      currentPhase: task?.current_phase || detail.current_phase || "",
      progressMessage: detail.progress_message || task?.progress_message || "",
      progress: normalizeProgress(detail.progress ?? task?.progress),
      lifecycleState: task?.lifecycle_state || "",
      status: task?.status || "",
      runtimeState: task?.runtime_state || "",
    };
  }

  function taskKey(task) {
    return (
      task?.id ||
      task?.child_session_key ||
      task?.tool_call_id ||
      task?.session_key ||
      task?.tool_name ||
      JSON.stringify({
        started_at: task?.started_at,
        updated_at: task?.updated_at,
        status: task?.status,
      })
    );
  }

  function isActiveTask(task) {
    var status = String(task?.status || "").toLowerCase();
    var lifecycle = String(task?.lifecycle_state || "").toLowerCase();
    return (
      status === "spawned" ||
      status === "running" ||
      lifecycle === "queued" ||
      lifecycle === "running" ||
      lifecycle === "verifying"
    );
  }

  function clearTaskIndicators() {
    taskStatusEl.innerHTML = "";
  }

  function buildTaskIndicator(task) {
    var detail = getTaskDetail(task);
    var title = detail.workflowKind || task?.tool_name || "Background task";
    var phase = detail.currentPhase || detail.lifecycleState || detail.status || "Running";
    var status = detail.lifecycleState || detail.status || detail.runtimeState || "running";
    var indicator = document.createElement("div");
    indicator.className = "session-task-indicator";
    indicator.setAttribute("data-testid", "session-task-indicator");
    indicator.dataset.taskKey = taskKey(task);
    indicator.dataset.sessionId = task?.session_key || currentSession;
    indicator.dataset.status = String(task?.status || "");
    indicator.dataset.lifecycleState = String(task?.lifecycle_state || "");

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

    var message = detail.progressMessage || "";
    if (!message) {
      message = phase ? humanize(phase) + "..." : statusLabel.textContent || "Working...";
    }

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
      var aTime = new Date(a?.updated_at || a?.started_at || 0).getTime();
      var bTime = new Date(b?.updated_at || b?.started_at || 0).getTime();
      return aTime - bTime;
    });

    taskStatusEl.innerHTML = "";
    if (activeTasks.length === 0) return;

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

  async function fetchJson(url, opts) {
    var resp = await fetch(url, opts);
    if (resp.status === 401) {
      showAuth();
      throw new Error("unauthorized");
    }
    if (!resp.ok) {
      throw new Error("HTTP " + resp.status);
    }
    return resp.json();
  }

  async function loadSessions() {
    try {
      var data = await fetchJson("/api/sessions", { headers: headers() });
      if (!Array.isArray(data)) {
        return [];
      }
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
        title.addEventListener("click", function () {
          selectSession(s.id);
        });

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

  async function loadHistory(id) {
    messagesEl.innerHTML = "";
    try {
      var msgs = await fetchJson(
        "/api/sessions/" + encodeURIComponent(id) + "/messages?limit=100",
        { headers: headers() },
      );
      if (!Array.isArray(msgs)) {
        return;
      }
      msgs.forEach(function (m) {
        if (m.media && m.media.length > 0) {
          m.media.forEach(function (path) {
            var name = path.split("/").pop() || "file";
            appendFileMessage(name, path, "");
          });
        } else {
          appendMessage(m.role.toLowerCase(), m.content);
        }
      });
    } catch (error) {
      // ignored — auth modal or transient error
    }
  }

  async function refreshTaskStatus(sessionId) {
    var requestSeq = ++taskRefreshSeq;
    try {
      var tasks = await fetchJson(
        "/api/sessions/" + encodeURIComponent(sessionId) + "/tasks",
        { headers: headers() },
      );
      if (requestSeq !== taskRefreshSeq || sessionId !== currentSession) return;
      if (!Array.isArray(tasks)) {
        clearTaskIndicators();
        return;
      }
      syncTasks(sessionId, tasks);
    } catch (error) {
      if (requestSeq === taskRefreshSeq && sessionId === currentSession) {
        clearTaskIndicators();
      }
    }
  }

  async function selectSession(id) {
    persistCurrentSession(id);
    loadSessions();
    await loadHistory(id);
    await refreshTaskStatus(id);
  }

  function deleteSession(id) {
    if (!id || !window.confirm('Delete session "' + id + '"?')) return;
    fetch("/api/sessions/" + encodeURIComponent(id), {
      method: "DELETE",
      headers: headers(),
    })
      .then(function (r) {
        if (r.status === 401) {
          showAuth();
          return;
        }
        if (id === currentSession) {
          persistCurrentSession("default");
          messagesEl.innerHTML = "";
          clearTaskIndicators();
        }
        loadSessions();
      })
      .catch(function () {});
  }

  function appendMessage(role, content) {
    var div = document.createElement("div");
    div.className = "message " + role;
    div.setAttribute("data-testid", role + "-message");

    var roleLabel = document.createElement("div");
    roleLabel.className = "role";
    roleLabel.textContent = role;
    div.appendChild(roleLabel);

    var body = document.createElement("div");
    body.textContent = content;
    div.appendChild(body);

    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return div;
  }

  function appendFileMessage(filename, path, caption) {
    var div = document.createElement("div");
    div.className = "message assistant";
    div.setAttribute("data-testid", "assistant-message");

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

  function parseSseChunk(buffer, text, handler) {
    buffer += text;
    var lines = buffer.split("\n");
    buffer = lines.pop();
    lines.forEach(function (line) {
      if (line.indexOf("data:") !== 0) return;
      var json = line.slice(5).trim();
      if (!json) return;
      try {
        handler(JSON.parse(json));
      } catch (error) {}
    });
    return buffer;
  }

  function finishStreaming() {
    sending = false;
    currentAbort = null;
    sendButton.disabled = false;
    cancelButton.classList.add("hidden");
  }

  cancelButton.addEventListener("click", function () {
    if (currentAbort) {
      currentAbort.abort();
    }
    finishStreaming();
  });

  authSubmitBtn.addEventListener("click", function () {
    token = authTokenEl.value.trim();
    sessionStorage.setItem(TOKEN_STORAGE_KEY, token);
    hideAuth();
    loadSessions();
    refreshTaskStatus(currentSession);
  });

  newSessionBtn.addEventListener("click", function () {
    var id = "s_" + Date.now();
    persistCurrentSession(id);
    messagesEl.innerHTML = "";
    clearTaskIndicators();
    loadSessions();
  });

  formEl.addEventListener("submit", function (e) {
    e.preventDefault();
    var text = inputEl.value.trim();
    if (!text || sending) return;

    sending = true;
    sendButton.disabled = true;
    cancelButton.classList.remove("hidden");
    inputEl.value = "";

    appendMessage("user", text);

    var assistantDiv = appendMessage("assistant", "");
    assistantDiv.classList.add("streaming");
    assistantDiv.dataset.testid = "assistant-message";
    var bodyEl = assistantDiv.querySelector("div:last-child");
    var accumulated = "";
    var sid = currentSession;
    var finished = false;
    currentAbort = new AbortController();

    function finish() {
      if (finished) return;
      finished = true;
      assistantDiv.classList.remove("streaming");
      finishStreaming();
    }

    fetch("/api/chat", {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ message: text, session_id: currentSession }),
      signal: currentAbort.signal,
    })
      .then(function (r) {
        if (r.status === 401) {
          showAuth();
          finish();
          return null;
        }
        if (!r.body) {
          finish();
          return null;
        }
        var reader = r.body.getReader();
        var decoder = new TextDecoder();
        var buf = "";

        function read() {
          reader.read().then(function (result) {
            if (result.done) {
              finish();
              return;
            }
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
                refreshTaskStatus(sid);
                if (data.has_bg_tasks) {
                  pollForBgFiles(sid);
                }
                finish();
              } else if (data.type === "file") {
                appendFileMessage(data.filename, data.path, data.caption);
              }
            });
            read();
          }).catch(function (err) {
            if (err && err.name === "AbortError") {
              finish();
              return;
            }
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
  });

  inputEl.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      formEl.dispatchEvent(new Event("submit"));
    }
  });

  function pollStatus() {
    fetch("/api/status", { headers: headers() })
      .then(function (r) {
        if (r.status === 401) {
          showAuth();
          return null;
        }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        var uptime = Math.floor(data.uptime_secs / 60);
        statusEl.textContent =
          data.model + " | " + data.provider + " | up " + uptime + "m | v" + data.version;
      })
      .catch(function () {
        statusEl.textContent = "Disconnected";
      });
  }

  function pollTaskStatus() {
    if (!currentSession) return;
    refreshTaskStatus(currentSession);
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
        .then(function (r) {
          return r.ok ? r.json() : null;
        })
        .then(function (msgs) {
          if (!msgs) {
            setTimeout(poll, 2000);
            return;
          }
          var done = false;
          msgs.forEach(function (m) {
            if (m.timestamp > startTime && m.media && m.media.length > 0) {
              m.media.forEach(function (path) {
                if (!delivered[path]) {
                  delivered[path] = true;
                  var name = path.split("/").pop() || "file";
                  appendFileMessage(name, path, "");
                }
              });
            }
            if (
              m.timestamp > startTime &&
              m.content &&
              (m.content.charAt(0) === "\u2713" || m.content.charAt(0) === "\u2717")
            ) {
              done = true;
            }
          });
          if (!done) setTimeout(poll, 2000);
        })
        .catch(function () {
          setTimeout(poll, 2000);
        });
    }

    setTimeout(poll, 2000);
  }

  async function bootstrap() {
    await loadSessions();
    await loadHistory(currentSession);
    await refreshTaskStatus(currentSession);
  }

  bootstrap();
  pollStatus();
  pollTaskStatus();
  setInterval(pollStatus, 30000);
  setInterval(pollTaskStatus, 2500);
})();
