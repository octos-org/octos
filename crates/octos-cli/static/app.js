(function () {
  "use strict";

  var token = sessionStorage.getItem("octos_token") || "";
  var currentSession = "default";
  var sending = false;

  var messagesEl = document.getElementById("messages");
  var inputEl = document.getElementById("input");
  var formEl = document.getElementById("chat-form");
  var sessionListEl = document.getElementById("session-list");
  var statusEl = document.getElementById("status-text");
  var newSessionBtn = document.getElementById("new-session");
  var authModal = document.getElementById("auth-modal");
  var authTokenEl = document.getElementById("auth-token");
  var authSubmitBtn = document.getElementById("auth-submit");

  function headers() {
    var h = { "Content-Type": "application/json" };
    if (token) h["Authorization"] = "Bearer " + token;
    return h;
  }

  function appendMessage(role, content) {
    var div = document.createElement("div");
    div.className = "message " + role;
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
    var roleLabel = document.createElement("div");
    roleLabel.className = "role";
    roleLabel.textContent = "assistant";
    div.appendChild(roleLabel);
    var body = document.createElement("div");
    var fileUrl = "/api/files?path=" + encodeURIComponent(path);
    var ext = (filename || "").split(".").pop().toLowerCase();
    if (ext === "mp3" || ext === "wav" || ext === "ogg" || ext === "m4a") {
      var audio = document.createElement("audio");
      audio.controls = true;
      audio.src = fileUrl;
      body.appendChild(audio);
      if (caption) {
        var cap = document.createElement("div");
        cap.textContent = caption;
        body.appendChild(cap);
      }
    } else {
      var a = document.createElement("a");
      a.href = fileUrl;
      a.download = filename;
      a.textContent = filename || "Download file";
      body.appendChild(a);
    }
    div.appendChild(body);
    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function showAuth() { authModal.classList.remove("hidden"); }
  function hideAuth() { authModal.classList.add("hidden"); }

  authSubmitBtn.addEventListener("click", function () {
    token = authTokenEl.value.trim();
    sessionStorage.setItem("octos_token", token);
    hideAuth();
    loadSessions();
    pollStatus();
  });

  // Sessions
  function loadSessions() {
    fetch("/api/sessions", { headers: headers() })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return null; }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        sessionListEl.innerHTML = "";
        data.forEach(function (s) {
          var li = document.createElement("li");
          li.dataset.id = s.id;
          li.dataset.sessionId = s.id;
          li.dataset.active = s.id === currentSession ? "true" : "false";
          if (s.id === currentSession) li.className = "active";
          li.addEventListener("click", function () { selectSession(s.id); });
          var title = document.createElement("span");
          title.className = "session-title";
          title.textContent = s.id + " (" + s.message_count + ")";
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
      })
      .catch(function () {});
  }

  function selectSession(id) {
    currentSession = id;
    loadSessions();
    loadHistory(id);
  }

  function deleteSession(id) {
    if (!id || !window.confirm("Delete session \"" + id + "\"?")) return;
    fetch("/api/sessions/" + encodeURIComponent(id), {
      method: "DELETE",
      headers: headers(),
    })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return; }
        if (id === currentSession) {
          currentSession = "default";
          messagesEl.innerHTML = "";
        }
        loadSessions();
      })
      .catch(function () {});
  }

  function loadHistory(id) {
    messagesEl.innerHTML = "";
    fetch("/api/sessions/" + encodeURIComponent(id) + "/messages?limit=100", { headers: headers() })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return null; }
        return r.json();
      })
      .then(function (msgs) {
        if (!msgs) return;
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
      })
      .catch(function () {});
  }

  newSessionBtn.addEventListener("click", function () {
    var id = "s_" + Date.now();
    currentSession = id;
    messagesEl.innerHTML = "";
    loadSessions();
  });

  // Parse SSE lines from a text chunk. Calls handler(jsonData) for each event.
  function parseSseChunk(buffer, text, handler) {
    buffer += text;
    var lines = buffer.split("\n");
    buffer = lines.pop(); // keep incomplete last line
    lines.forEach(function (line) {
      if (line.indexOf("data:") !== 0) return;
      var json = line.slice(5).trim();
      if (!json) return;
      try { handler(JSON.parse(json)); } catch (e) {}
    });
    return buffer;
  }

  // Chat — POST returns SSE from gateway; read with fetch+ReadableStream
  formEl.addEventListener("submit", function (e) {
    e.preventDefault();
    var text = inputEl.value.trim();
    if (!text || sending) return;
    sending = true;
    formEl.querySelector("button").disabled = true;
    inputEl.value = "";

    appendMessage("user", text);

    var assistantDiv = appendMessage("assistant", "");
    assistantDiv.classList.add("streaming");
    var bodyEl = assistantDiv.querySelector("div:last-child");
    var accumulated = "";
    var sid = currentSession;
    var finished = false;

    function finish() {
      if (finished) return;
      finished = true;
      assistantDiv.classList.remove("streaming");
      sending = false;
      formEl.querySelector("button").disabled = false;
    }

    fetch("/api/chat", {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ message: text, session_id: currentSession }),
    })
      .then(function (r) {
        if (r.status === 401) { showAuth(); finish(); return; }
        if (!r.body) { finish(); return; }
        var reader = r.body.getReader();
        var decoder = new TextDecoder();
        var buf = "";

        function read() {
          reader.read().then(function (result) {
            if (result.done) { finish(); return; }
            buf = parseSseChunk(buf, decoder.decode(result.value, { stream: true }), function (data) {
              if (data.type === "keepalive") return;
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
                finish();
                if (data.has_bg_tasks) {
                  pollForBgFiles(sid);
                }
              } else if (data.type === "file") {
                appendFileMessage(data.filename, data.path, data.caption);
              }
            });
            read();
          }).catch(function () { finish(); });
        }
        read();
      })
      .catch(function (err) {
        bodyEl.textContent = "Error: " + err.message;
        finish();
      });
  });

  // Enter to send, Shift+Enter for newline
  inputEl.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      formEl.dispatchEvent(new Event("submit"));
    }
  });

  // Status polling
  function pollStatus() {
    fetch("/api/status", { headers: headers() })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return null; }
        return r.json();
      })
      .then(function (data) {
        if (!data) return;
        var uptime = Math.floor(data.uptime_secs / 60);
        statusEl.textContent = data.model + " | " + data.provider + " | up " + uptime + "m | v" + data.version;
      })
      .catch(function () {
        statusEl.textContent = "Disconnected";
      });
  }

  // Poll session history for background task files (TTS, slides, etc.)
  function pollForBgFiles(sessionId) {
    var startTime = new Date().toISOString();
    var attempts = 0;
    var maxAttempts = 150;
    var delivered = {};

    function poll() {
      if (attempts++ >= maxAttempts) return;
      fetch("/api/sessions/" + encodeURIComponent(sessionId) + "/messages?limit=100", { headers: headers() })
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
                  appendFileMessage(name, path, "");
                }
              });
            }
            if (m.timestamp > startTime && (m.content.charAt(0) === "\u2713" || m.content.charAt(0) === "\u2717")) {
              done = true;
            }
          });
          if (!done) setTimeout(poll, 2000);
        })
        .catch(function () { setTimeout(poll, 2000); });
    }

    setTimeout(poll, 2000);
  }

  // Init
  loadSessions();
  pollStatus();
  setInterval(pollStatus, 30000);
})();
