// App Stores section: configure the YAML app stores the "Install app from
// store" flow browses.
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

// --- App stores -----------------------------------------------------------
async function renderStores() {
  const res = await request("/api/admin/apps/stores", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("stores.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const stores = res.data || [];
  const rows = stores
    .map(
      (s) => `
      <tr>
        <td><strong>${esc(s.name)}</strong></td>
        <td><code>${esc(s.url)}</code></td>
        <td class="admin-actions"><button class="btn btn-sm btn-danger" data-store="${esc(s.name)}">${t("common.delete")}</button></td>
      </tr>`
    )
    .join("");
  setPanel(`
    <div class="card">
      <h3>${t("stores.addTitle")}</h3>
      <div class="admin-form-row" style="margin-bottom:12px;">
        <div class="field"><label>${t("common.name")}</label><input type="text" id="store-name"></div>
        <div class="field" style="flex:2;"><label>${t("stores.urlLabel")}</label><input type="text" id="store-url" placeholder="${t("stores.urlPlaceholder")}"></div>
      </div>
      <button class="btn btn-primary" id="store-create">${t("stores.add")}</button>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">${t("stores.configuredTitle")}</h3>
      <table class="admin-table">
        <thead><tr><th>${t("common.name")}</th><th>${t("stores.url")}</th><th></th></tr></thead>
        <tbody id="store-rows">${rows || `<tr><td colspan="3" class="muted">${t("stores.empty")}</td></tr>`}</tbody>
      </table>
    </div>
  `);
  $("store-create").addEventListener("click", async () => {
    const name = $("store-name").value.trim();
    const url = $("store-url").value.trim();
    if (!name || !url) return toast(t("stores.nameUrlRequired"), "error");
    const r = await request("/api/admin/apps/stores", "POST", { name, url });
    if (r.error) return toast(r.error.message, "error");
    toast(t("stores.added"));
    render();
  });
  $("store-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-store]");
    if (!btn) return;
    if (!(await confirmModal(t("stores.confirmDelete", { name: btn.dataset.store }), { title: t("stores.confirmDeleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
    const r = await request(`/api/admin/apps/stores/${encodeURIComponent(btn.dataset.store)}`, "DELETE");
    if (r.error) return toast(r.error.message, "error");
    toast(t("stores.deleted"));
    render();
  });
}

export { renderStores };
