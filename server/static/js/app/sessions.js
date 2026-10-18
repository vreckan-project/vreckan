// Sessions list (main view): render, refresh, per-session file send, and the
// per-session Share action (ANT-307).
import { $, toast, formatLogoSrc, uploadFileChunks } from "../util.js";
import { secureFetch } from "../api.js";
import { t } from "../i18n.js";
import { openSessionShareModal } from "./session-share.js";

function renderSessions(sessions, sharesBySession = {}) {
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
    const activeShares = (sharesBySession[s.session_id] || []).filter(
      (sh) => !sh.revoked && (!sh.expiry_timestamp || sh.expiry_timestamp * 1000 > Date.now())
    );
    if (activeShares.length) {
      const badge = document.createElement("button");
      badge.type = "button";
      badge.className = "session-share-badge";
      badge.textContent =
        activeShares.length === 1
          ? t("shareSession.linkCount", { count: activeShares.length })
          : t("shareSession.linkCountPlural", { count: activeShares.length });
      badge.title = t("shareSession.linksBadgeTitle");
      badge.addEventListener("click", () => openSessionShareModal(s, { manage: true }));
      metaLine.appendChild(badge);
    }
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

    const share = document.createElement("button");
    share.className = "btn btn-ghost btn-sm";
    share.textContent = t("common.share");
    share.title = t("shareSession.shareTitle");
    share.addEventListener("click", () => openSessionShareModal(s));
    actions.appendChild(share);

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

// Fetch the share list for each session (in parallel, best-effort) so the
// row badge can show how many active share links a session has.
async function fetchSessionShares(sessions) {
  const results = await Promise.allSettled(
    sessions.map(async (s) => {
      const shares = await secureFetch(`/api/sessions/${s.session_id}/shares`, { method: "GET" });
      return [s.session_id, Array.isArray(shares) ? shares : []];
    })
  );
  const bySession = {};
  for (const r of results) {
    if (r.status === "fulfilled") bySession[r.value[0]] = r.value[1];
  }
  return bySession;
}

export async function refreshShareBadges(sessions) {
  const list = $("sessions-list");
  if (!list) return;
  const sharesBySession = await fetchSessionShares(Array.isArray(sessions) ? sessions : []);
  renderSessions(Array.isArray(sessions) ? sessions : [], sharesBySession);
}

async function loadSessions() {
  try {
    const sessions = await secureFetch("/api/sessions", { method: "GET" });
    const safe = Array.isArray(sessions) ? sessions : [];
    const sharesBySession = await fetchSessionShares(safe);
    renderSessions(safe, sharesBySession);
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
