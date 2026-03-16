(function () {
  "use strict";

  var token = sessionStorage.getItem("crew_token") || "";
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

  function escapeHtml(s) {
    var div = document.createElement("div");
    div.textContent = s;
    return div.innerHTML;
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

  function showAuth() {
    authModal.classList.remove("hidden");
  }

  function hideAuth() {
    authModal.classList.add("hidden");
  }

  authSubmitBtn.addEventListener("click", function () {
    token = authTokenEl.value.trim();
    sessionStorage.setItem("crew_token", token);
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
          li.textContent = s.id + " (" + s.message_count + ")";
          li.dataset.id = s.id;
          if (s.id === currentSession) li.className = "active";
          li.addEventListener("click", function () { selectSession(s.id); });
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
          appendMessage(m.role.toLowerCase(), m.content);
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

  // Chat
  formEl.addEventListener("submit", function (e) {
    e.preventDefault();
    var text = inputEl.value.trim();
    if (!text || sending) return;
    sending = true;
    formEl.querySelector("button").disabled = true;
    inputEl.value = "";

    appendMessage("user", text);

    // Create streaming placeholder
    var assistantDiv = appendMessage("assistant", "");
    assistantDiv.classList.add("streaming");
    var bodyEl = assistantDiv.querySelector("div:last-child");

    // Start SSE listener before sending request
    var evtSource = new EventSource("/api/chat/stream");
    var accumulated = "";

    evtSource.onmessage = function (event) {
      try {
        var data = JSON.parse(event.data);
        if (data.type === "token" && data.text) {
          accumulated += data.text;
          bodyEl.textContent = accumulated;
          messagesEl.scrollTop = messagesEl.scrollHeight;
        } else if (data.type === "stream_end") {
          // Stream done, wait for POST response
        }
      } catch (err) {}
    };

    fetch("/api/chat", {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ message: text, session_id: currentSession }),
    })
      .then(function (r) {
        if (r.status === 401) { showAuth(); return null; }
        return r.json();
      })
      .then(function (data) {
        evtSource.close();
        assistantDiv.classList.remove("streaming");
        if (data && data.content) {
          bodyEl.textContent = data.content;
        }
        messagesEl.scrollTop = messagesEl.scrollHeight;
        loadSessions();
      })
      .catch(function (err) {
        evtSource.close();
        assistantDiv.classList.remove("streaming");
        bodyEl.textContent = "Error: " + err.message;
      })
      .finally(function () {
        sending = false;
        formEl.querySelector("button").disabled = false;
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

  // Init
  loadSessions();
  pollStatus();
  setInterval(pollStatus, 30000);
})();
