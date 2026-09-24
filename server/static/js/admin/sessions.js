// Sessions section: the per-user list of active sessions, with Connect
// (open in a new tab) and Kill actions.
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
                <td><strong>${esc(s.name ? `${s.app_name || s.app_id} - ${s.name}` : (s.app_name || s.app_id))}</strong><div class="muted" style="font-size:0.78rem;">${esc(s.session_id)}</div></td>
                <td>${s.created_at ? new Date(s.created_at * 1000).toLocaleString() : "—"}</td>
                <td>${s.is_collaboration ? t("common.yes") : t("common.no")}</td>
                <td class="admin-actions">
                  <button class="btn btn-sm btn-ghost" data-connect="${esc(s.session_id)}">${t("common.connect")}</button>
                  <button class="btn btn-sm btn-danger" data-kill="${esc(s.session_id)}">${t("sessions.kill")}</button>
                </td>
              </tr>`
            )
            .join("");
          return `
            <div class="card">
              <h3>${esc(g.username)}</h3>
              <table class="admin-table" style="margin-top:8px;">
                <thead><tr><th>${t("sessions.session")}</th><th>${t("sessions.started")}</th><th>${t("sessions.roomMode")}</th><th></th></tr></thead>
                <tbody>${rows || `<tr><td colspan="4" class="muted">${t("sessions.noActive")}</td></tr>`}</tbody>
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
    const btn = e.target.closest("button[data-kill]");
    if (!btn) return;
    if (!(await confirmModal(t("sessions.killConfirm"), { title: t("sessions.killTitle"), confirmLabel: t("sessions.kill"), danger: true }))) return;
    const r = await request(`/api/admin/sessions/${encodeURIComponent(btn.dataset.kill)}`, "DELETE");
    if (r.error) return toast(r.error.message, "error");
    toast(t("sessions.killed"));
    render();
  });
}

export { renderSessions };
