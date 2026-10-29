// Sessions section: the per-user list of active sessions, with Connect
// (open in a new tab) and Kill actions. ANT-654: rows render the recorded
// background lifecycle state (starting / failed to start), Kill is
// optimistic, and Recreate polls for background readiness before settling.
//
// The small DOM helpers ($, esc, toast, request, setPanel) are local copies so
// this file is self-contained; `render` comes from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal } from "../modal.js";
import { t } from "../i18n.js";
import { render } from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- Sessions -------------------------------------------------------------
async function renderSessions() {
  const res = await request("/api/admin/sessions", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("sessions.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const groups = res.data || [];
  // Map of session_id -> session URL (with access token) so the Connect
  // button can open any user's session in a new tab.
  const urlMap = {};
  for (const g of groups) {
    for (const s of g.sessions || []) {
      if (s.session_id && s.session_url) urlMap[s.session_id] = s.session_url;
    }
  }
  const html = groups.length
    ? groups
        .map((g) => {
          const rows = (g.sessions || [])
            .map(
              (s) => `
              <tr>
                <td><strong>${esc(s.name ? `${s.app_name || s.app_id} - ${s.name}` : (s.app_name || s.app_id))}</strong><div class="muted fs-xs" >${esc(s.session_id)}</div></td>
                <td>${s.created_at ? new Date(s.created_at * 1000).toLocaleString() : "—"}</td>
                <td>${s.is_collaboration ? t("common.yes") : t("common.no")}</td>
                <td>${s.out_of_date ? `<span class="badge badge-warn">${t("sessions.outOfDate")}</span>` : ""}${sessionStateBadge(s)}</td>
                <td class="admin-actions">
                  <button class="btn btn-sm btn-ghost" data-connect="${esc(s.session_id)}">${t("common.connect")}</button>
                  ${s.out_of_date ? `<button class="btn btn-sm btn-ghost" data-recreate="${esc(s.session_id)}">${t("sessions.recreate")}</button>` : ""}
                  <button class="btn btn-sm btn-danger" data-kill="${esc(s.session_id)}">${t("sessions.kill")}</button>
                </td>
              </tr>`
            )
            .join("");
          return `
            <div class="card">
              <h3>${esc(g.username)}</h3>
              <table class="admin-table mt-8" >
                <thead><tr><th>${t("sessions.session")}</th><th>${t("sessions.started")}</th><th>${t("sessions.roomMode")}</th><th>${t("sessions.image")}</th><th></th></tr></thead>
                <tbody>${rows || `<tr><td colspan="5" class="muted">${t("sessions.noActive")}</td></tr>`}</tbody>
              </table>
            </div>`;
        })
        .join("")
    : `<div class="card"><h3>${t("sessions.heading")}</h3><p class="muted">${t("sessions.noActiveUsers")}</p></div>`;
  setPanel(`<div id="session-cards">${html}</div>`);
  const wrap = $("session-cards");
  wrap.addEventListener("click", async (e) => {
    const connectBtn = e.target.closest("button[data-connect]");
    if (connectBtn) {
      const url = urlMap[connectBtn.dataset.connect];
      if (url) window.open(new URL(url, window.location.origin).href, "_blank");
      return;
    }
    const recreateBtn = e.target.closest("button[data-recreate]");
    if (recreateBtn) {
      recreateBtn.disabled = true;
      const orig = recreateBtn.textContent;
      recreateBtn.textContent = t("sessions.recreating");
      const r = await request(`/api/admin/sessions/${encodeURIComponent(recreateBtn.dataset.recreate)}/recreate`, "POST");
      if (r.error) {
        recreateBtn.disabled = false;
        recreateBtn.textContent = orig;
        return toast(r.error.message, "error");
      }
      // ANT-654: the response no longer waits for readiness (the containers
      // are created/started; the readiness wait runs in the background). The
      // user-scoped status endpoint only answers for the caller's own
      // sessions, so poll the admin list — every row carries the recorded
      // state — until the target session settles, then reconcile.
      toast(t("launch.sessionStarting"));
      const outcome = await pollAdminSessionSettled(recreateBtn.dataset.recreate);
      if (outcome.state === "ready") toast(t("launch.sessionReady"));
      else if (outcome.state === "failed")
        toast(t("sessions.launchFailedToast", { error: outcome.error || t("sessions.failedToStart") }), "error");
      else if (outcome.state === "gone") toast(t("sessions.goneWhileStarting"), "info");
      else toast(t("sessions.stillStarting"), "info");
      render();
      return;
    }
    const btn = e.target.closest("button[data-kill]");
    if (!btn) return;
    if (!(await confirmModal(t("sessions.killConfirm"), { title: t("sessions.killTitle"), confirmLabel: t("sessions.kill"), danger: true }))) return;
    // ANT-654: the DELETE returns as soon as the session is gone from the
    // live list; the container teardown runs in a background task. The
    // button flips to "Stopping…" immediately and the re-render below
    // reconciles (restoring the row if the DELETE fails).
    btn.disabled = true;
    btn.textContent = t("sessions.stopping");
    const r = await request(`/api/admin/sessions/${encodeURIComponent(btn.dataset.kill)}`, "DELETE");
    if (r.error) {
      render();
      return toast(r.error.message, "error");
    }
    toast(t("sessions.killed"));
    render();
  });
}

// ANT-654: the recorded readiness state, rendered inline in the Image
// column. The backend reads pre-ANT-654 rows (no `ready` flag) as ready,
// so only the new intermediate states produce a badge.
function sessionStateBadge(s) {
  if (s.ready === false && !s.launch_failed) {
    return `<span class="session-state session-state-starting">${t("sessions.starting")}</span>`;
  }
  if (s.launch_failed === true) {
    const text = s.launch_error
      ? t("sessions.failedToStartDetail", { error: s.launch_error })
      : t("sessions.failedToStart");
    return `<span class="session-state session-state-failed" title="${esc(s.launch_error || "")}">${esc(text)}</span>`;
  }
  return "";
}

// ANT-654: wait for the background readiness wait of a recreated session to
// settle, as recorded on its admin-list row (the user-scoped status endpoint
// only answers for the caller's own sessions, so it cannot be used for
// another user's session). The backend wait times out at 60 s per container
// (recreate waits sequentially), so the client caps at 120 s.
async function pollAdminSessionSettled(sessionId) {
  const POLL_MS = 2500;
  const TIMEOUT_MS = 120000;
  const startedAt = Date.now();
  for (;;) {
    await new Promise((r) => setTimeout(r, POLL_MS));
    const res = await request("/api/admin/sessions", "GET");
    if (!res.error) {
      for (const g of res.data || []) {
        const s = (g.sessions || []).find((x) => x.session_id === sessionId);
        if (!s) return { state: "gone" };
        if (s.ready === true) return { state: "ready" };
        if (s.launch_failed === true) return { state: "failed", error: s.launch_error || "" };
      }
    }
    if (Date.now() - startedAt >= TIMEOUT_MS) return { state: "timeout" };
  }
}

export { renderSessions };
