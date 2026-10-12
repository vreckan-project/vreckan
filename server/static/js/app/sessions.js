// Sessions list (main view): render, refresh, and per-session file send.
import { $, toast, formatLogoSrc, uploadFileChunks } from "../util.js";
import { secureFetch } from "../api.js";
import { t } from "../i18n.js";

function renderSessions(sessions) {
  const list = $("sessions-list");
  list.innerHTML = "";
  if (!sessions.length) {
    list.innerHTML = `<p class="muted">${t("launch.noSessions")}</p>`;
    return;
  }
  for (const s of sessions) {
    const item = document.createElement("div");
    item.className = "session-item";

    const img = document.createElement("img");
    img.alt = "app logo";
    img.src = s.app_logo || "/img/icon128.png";
    if (s.app_logo && s.app_logo.startsWith("/api/app_icon/")) {
      formatLogoSrc({ logo: s.app_logo }).then((src) => {
        img.src = src;
      });
    }
    item.appendChild(img);

    const info = document.createElement("div");
    info.className = "session-info";
    const name = document.createElement("div");
    name.className = "app-name";
    name.textContent = s.name ? `${s.app_name} - ${s.name}` : s.app_name;
    info.appendChild(name);
    const metaLine = document.createElement("div");
    metaLine.className = "session-meta";
    const when = new Date(s.created_at * 1000);
    metaLine.textContent = when.toLocaleString();
    if (s.is_collaboration) metaLine.textContent += ` · ${t("sessions.roomMode")}`;
    if (s.out_of_date) metaLine.textContent += ` · ${t("sessions.outOfDate")}`;
    info.appendChild(metaLine);
    item.appendChild(info);

    const actions = document.createElement("div");
    actions.className = "session-actions";

    const open = document.createElement("a");
    open.className = "btn btn-ghost btn-sm";
    open.href = s.session_url;
    open.target = "_blank";
    open.rel = "noopener";
    open.textContent = t("common.open");
    actions.appendChild(open);

    if (s.out_of_date) {
      const recreate = document.createElement("button");
      recreate.className = "btn btn-ghost btn-sm";
      recreate.textContent = t("sessions.recreate");
      recreate.title = t("sessions.recreateTitle");
      recreate.addEventListener("click", async () => {
        recreate.disabled = true;
        recreate.textContent = t("sessions.recreating");
        try {
          await secureFetch(`/api/sessions/${s.session_id}/recreate`, { method: "POST" });
          toast(t("sessions.recreated"));
          loadSessions();
        } catch (e) {
          recreate.disabled = false;
          recreate.textContent = t("sessions.recreate");
          toast(e.message, "error");
        }
      });
      actions.appendChild(recreate);
    }

    const sendFile = document.createElement("button");
    sendFile.className = "btn btn-ghost btn-sm";
    sendFile.textContent = t("common.sendFile");
    sendFile.title = t("sessions.sendFileTitle");
    sendFile.addEventListener("click", () => pickFileForSession(s.session_id, sendFile));
    actions.appendChild(sendFile);

    const kill = document.createElement("button");
    kill.className = "btn btn-danger btn-sm";
    kill.textContent = t("sessions.kill");
    kill.addEventListener("click", async () => {
      try {
        await secureFetch(`/api/sessions/${s.session_id}`, { method: "DELETE" });
        toast(t("sessions.terminated"));
        loadSessions();
      } catch (e) {
        toast(e.message, "error");
      }
    });
    actions.appendChild(kill);
    item.appendChild(actions);

    list.appendChild(item);
  }
}

async function loadSessions() {
  try {
    const sessions = await secureFetch("/api/sessions", { method: "GET" });
    renderSessions(Array.isArray(sessions) ? sessions : []);
  } catch (e) {
    if (!e.message.includes("401")) toast(`Sessions: ${e.message}`, "error");
  }
}

// ---------------------------------------------------------------------------
// Feat 9 — send a file to an active session
// ---------------------------------------------------------------------------
// "Send File" on a session row: open the shared file picker, then chunk-upload
// the chosen file and hand it to the session via /api/sessions/{id}/send_file.
// The button doubles as the progress indicator (Uploading i/n…).
function pickFileForSession(sessionId, button) {
  const input = $("session-send-file");
  input.value = "";
  input.onchange = () => {
    const file = input.files && input.files[0];
    input.onchange = null;
    if (!file) return;
    sendFileToSession(sessionId, file, button);
  };
  input.click();
}

async function sendFileToSession(sessionId, file, button) {
  const original = button.textContent;
  button.disabled = true;
  try {
    const { upload_id, total_chunks } = await uploadFileChunks(file, (done, total) => {
      button.textContent = t("sessions.uploading", { done, total });
    });
    button.textContent = t("sessions.sending");
    const res = await secureFetch(`/api/sessions/${sessionId}/send_file`, {
      method: "POST",
      body: { filename: file.name, upload_id, total_chunks },
    });
    toast(res && res.message ? res.message : t("sessions.fileSent", { name: file.name }));
  } catch (e) {
    toast(e.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

export { renderSessions, loadSessions, pickFileForSession, sendFileToSession };
