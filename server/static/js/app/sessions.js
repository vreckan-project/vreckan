// Sessions list (main view): render, refresh, per-session file send, the
// per-session Share action (ANT-307), and the per-session Join action
// (ANT-440 — "Join a running session" for a second user).
import { $, toast, formatLogoSrc, uploadFileChunks } from "../util.js";
import { secureFetch } from "../api.js";
import { t } from "../i18n.js";
import { confirmModal } from "../modal.js";
import { openSessionShareModal } from "./session-share.js";
import { installedApps, renderAppApps } from "../app.js";

// Last known session list (backend order: newest first). The extended
// backend lists every running session, each flagged `is_mine`; the app
// cards read this state to show their Join affordance (ANT-440).
let currentSessions = [];
export function getSessions() {
  return currentSessions;
}

// The session a Join action would target for an app: the most recently
// launched running session the caller can join (backend order is newest
// first). Used by the app cards (ANT-440); null when the app has no
// joinable running session.
export function joinTargetForApp(appId) {
  return currentSessions.find((s) => s.app_id === appId && isJoinable(s)) || null;
}

// The backend marks each entry `is_mine`; older backends return only the
// caller's own sessions without the field, in which case every entry is
// ours and behavior is unchanged.
function isMine(s) {
  return s.is_mine !== false;
}

// Can the caller join this session? The backend's `joinable` field is the
// source of truth (app access + v1-joinable plain running session); when it
// is absent (older backend) compute it: a plain session of an app in the
// caller's catalog.
function isJoinable(s) {
  if (s.joinable !== undefined) return s.joinable === true;
  if (s.is_collaboration) return false;
  return (installedApps || []).some((a) => a.id === s.app_id);
}

function sessionDisplayName(s) {
  return s.name ? `${s.app_name} - ${s.name}` : s.app_name;
}

// Pre-join confirmation (ANT-440): state the v1 participant role and the
// observed WS behavior (review-gate evidence of 2026-10-20): with the
// default WebSockets transport the current driver's live stream ends while
// the joiner is connected (session and token survive, re-entry at any
// time); the joiner may need a refresh to gain keyboard/mouse control
// (selkies 2.0.0 MK_ACCESS quirk, ANT-443).
async function confirmJoin(s) {
  return confirmModal(t("sessions.joinConfirmMsg", { name: sessionDisplayName(s) }), {
    title: t("sessions.joinConfirmTitle"),
    confirmLabel: t("sessions.join"),
  });
}

// The Join action: POST /api/sessions/{id}/join mints the caller their own
// participant token through the share-grant path (ANT-440 contract — no
// share link, no owner URL), then the returned URL is opened in a new tab.
export async function joinSession(s, button) {
  if (!(await confirmJoin(s))) return;
  button.disabled = true;
  const original = button.textContent;
  button.textContent = t("sessions.joining");
  try {
    const res = await secureFetch(`/api/sessions/${s.session_id}/join`, {
      method: "POST",
      body: {},
    });
    const url = res && (res.session_url || res.url);
    if (url) {
      window.open(url, "_blank", "noopener");
      toast(t("sessions.joined"));
      loadSessions();
    } else {
      toast(t("sessions.joinNoUrl"), "error");
    }
  } catch (e) {
    toast(joinErrorMessage(e), "error");
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

// One toast per failure mode (ANT-440).
function joinErrorMessage(e) {
  const msg = String((e && e.message) || e);
  if (msg.includes("status: 404") || msg.includes("status: 410")) return t("sessions.joinEnded");
  if (msg.includes("status: 403")) return t("sessions.joinNoAccess");
  if (msg.includes("status: 409")) return t("sessions.joinRoom");
  return t("sessions.joinFailed", { message: msg });
}

function makeJoinButton(s) {
  const join = document.createElement("button");
  join.type = "button";
  join.className = "btn btn-primary btn-sm session-join-btn";
  join.textContent = t("sessions.join");
  join.title = t("sessions.joinTitle");
  join.addEventListener("click", () => joinSession(s, join));
  return join;
}

function renderSessions(sessions, sharesBySession = {}) {
  currentSessions = Array.isArray(sessions) ? sessions : [];
  const list = $("sessions-list");
  list.innerHTML = "";
  if (!currentSessions.length) {
    list.innerHTML = `<p class="muted">${t("launch.noSessions")}</p>`;
    return;
  }
  for (const s of currentSessions) {
    const mine = isMine(s);
    // Other users' rows appear only when joinable (rooms and no-access apps
    // have no v1 Join affordance, so there is nothing for the row to offer).
    if (!mine && !isJoinable(s)) continue;

    const item = document.createElement("div");
    item.className = "session-item" + (mine ? "" : " session-other");

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
    name.textContent = sessionDisplayName(s);
    info.appendChild(name);
    const metaLine = document.createElement("div");
    metaLine.className = "session-meta";
    const when = new Date(s.created_at * 1000);
    metaLine.textContent = when.toLocaleString();
    if (!mine && s.username) metaLine.textContent += ` · ${t("sessions.ownerBy", { owner: s.username })}`;
    if (s.is_collaboration) metaLine.textContent += ` · ${t("sessions.roomMode")}`;
    if (s.out_of_date) metaLine.textContent += ` · ${t("sessions.outOfDate")}`;
    if (mine) {
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
      // Participants (ANT-467): who has joined this session. The list API
      // carries the joiners' usernames for the caller's own sessions, so no
      // extra fetch is needed for the badge; the manage modal shows the
      // full list with per-joiner revoke.
      const participants = Array.isArray(s.participants) ? s.participants : [];
      if (participants.length) {
        const badge = document.createElement("button");
        badge.type = "button";
        badge.className = "session-share-badge session-participants-badge";
        badge.textContent =
          participants.length === 1
            ? t("sessions.participantCount", { count: participants.length })
            : t("sessions.participantCountPlural", { count: participants.length });
        badge.title = t("sessions.participantsBadgeTitle", { names: participants.join(", ") });
        badge.addEventListener("click", () => openSessionShareModal(s, { manage: true }));
        metaLine.appendChild(badge);
      }
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
    if (mine) actions.appendChild(open);

    // Join (ANT-440): other users' rows only — on own rows it is redundant
    // with Open (ANT-618). Absent on room sessions (v1) and when not
    // joinable.
    if (!mine && isJoinable(s)) actions.appendChild(makeJoinButton(s));

    if (mine) {
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
            // F4-followup (ANT-471): the encrypted empty body is the CSRF gate —
            // a cross-site form POST without a crypto session is rejected 400.
            await secureFetch(`/api/sessions/${s.session_id}/recreate`, {
              method: "POST",
              body: {},
            });
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
          // F4-followup (ANT-471): the encrypted empty body is the CSRF gate —
          // a cross-site form POST/DELETE without a crypto session is rejected 400.
          await secureFetch(`/api/sessions/${s.session_id}`, {
            method: "DELETE",
            body: {},
          });
          toast(t("sessions.terminated"));
          loadSessions();
        } catch (e) {
          toast(e.message, "error");
        }
      });
      actions.appendChild(kill);
    }
    item.appendChild(actions);

    list.appendChild(item);
  }
}

// Fetch the share list for the caller's own sessions (in parallel,
// best-effort) so the row badge can show how many active share links a
// session has. Other users' share metadata is not the joiner's concern.
async function fetchSessionShares(sessions) {
  const mine = sessions.filter(isMine);
  const results = await Promise.allSettled(
    mine.map(async (s) => {
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
  const safe = Array.isArray(sessions) ? sessions : [];
  const sharesBySession = await fetchSessionShares(safe);
  renderSessions(safe, sharesBySession);
}

async function loadSessions() {
  try {
    const sessions = await secureFetch("/api/sessions", { method: "GET" });
    const safe = Array.isArray(sessions) ? sessions : [];
    const sharesBySession = await fetchSessionShares(safe);
    renderSessions(safe, sharesBySession);
    // Keep the app cards' Join affordance in step with the session list
    // (ANT-440) — a session starting or dying changes what the tiles offer.
    renderAppApps(installedApps);
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
