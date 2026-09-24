// Roles section: the role table, the "Create role" form, and the edit modal.
// Roles bundle fine-grained permissions and can be assigned to users and
// groups. Built-in roles (admin / operator / user) can be edited but not
// deleted; custom roles can be created and deleted (deletion is refused while
// a role is still assigned to a user or group).
//
// The small DOM helpers ($, esc, toast, request) come from util.js; the
// cross-section helpers come from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal } from "../modal.js";
import { t } from "../i18n.js";
import {
  loadPermCatalog,
  permCheckboxesHtml,
  readPermCheckboxes,
  render,
} from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- Roles ----------------------------------------------------------------
async function renderRoles() {
  const [rolesRes, permRes] = await Promise.all([
    request("/api/admin/roles", "GET"),
    loadPermCatalog(),
  ]);
  if (rolesRes.error) {
    setPanel(`<div class="card"><h3>${t("roles.heading")}</h3><div class="error">${esc(rolesRes.error.message)}</div></div>`);
    return;
  }
  const roles = rolesRes.data || [];
  const rows = roles
    .map(
      (r) => `
      <tr>
        <td><strong>${esc(r.name)}</strong>${r.is_builtin ? ` <span class="badge">${t("roles.builtin")}</span>` : ""}</td>
        <td class="muted">${esc(r.description || "")}</td>
        <td>${(r.permissions || []).length ? (r.permissions || []).map((p) => `<span class="badge">${esc(p)}</span>`).join(" ") : `<span class="muted">${t("common.none")}</span>`}</td>
        <td class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-act="edit" data-name="${esc(r.name)}">${t("common.edit")}</button>
          <button class="btn btn-sm btn-danger" data-act="delete" data-name="${esc(r.name)}" ${r.is_builtin ? "disabled" : ""}>${t("common.delete")}</button>
        </td>
      </tr>`
    )
    .join("");

  setPanel(`
    <div class="card">
      <h3>${t("roles.createTitle")}</h3>
      <div class="admin-form-row" style="margin-bottom:12px;">
        <div class="field"><label>${t("common.name")}</label><input type="text" id="role-new-name" placeholder="^[a-zA-Z0-9_-]+$"></div>
        <div class="field" style="flex:2;"><label>${t("roles.description")}</label><input type="text" id="role-new-desc"></div>
      </div>
      <div id="role-new-perms">${permCheckboxesHtml()}</div>
      <div style="margin-top:12px;"><button class="btn btn-primary" id="role-create">${t("common.create")}</button></div>
      <div id="role-result"></div>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">${t("roles.heading")}</h3>
      <table class="admin-table">
        <thead><tr><th>${t("common.name")}</th><th>${t("roles.description")}</th><th>${t("roles.permissions")}</th><th></th></tr></thead>
        <tbody id="role-rows">${rows || `<tr><td colspan="4" class="muted">${t("roles.empty")}</td></tr>`}</tbody>
      </table>
    </div>
    <div id="role-modal"></div>
  `);

  $("role-create").addEventListener("click", async () => {
    const name = $("role-new-name").value.trim();
    if (!name) return toast(t("roles.nameRequired"), "error");
    const description = $("role-new-desc").value.trim();
    const permissions = readPermCheckboxes($("role-new-perms"));
    const res = await request("/api/admin/roles", "POST", { name, description, permissions });
    if (res.error) {
      $("role-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(res.error.message)}</div>`;
      return;
    }
    toast(t("roles.created"));
    render();
  });

  $("role-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const name = btn.dataset.name;
    if (btn.dataset.act === "delete") {
      if (!(await confirmModal(t("roles.confirmDelete", { name }), { title: t("roles.confirmDeleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
      const res = await request(`/api/admin/roles/${encodeURIComponent(name)}`, "DELETE");
      if (res.error) return toast(res.error.message, "error");
      toast(t("roles.deleted"));
      render();
      return;
    }
    if (btn.dataset.act === "edit") {
      const d = await request("/api/admin/roles", "GET");
      const role = (d.data || []).find((r) => r.name === name);
      if (!role) return toast(t("accounts.toastNotFound"), "error");
      $("role-modal").innerHTML = `
        <div class="card" style="margin-top:16px;">
          <h3>${t("roles.editTitle", { name: esc(name) })}</h3>
          <div class="field" style="margin-bottom:12px;"><label>${t("roles.description")}</label><input type="text" id="role-edit-desc" value="${esc(role.description || "")}"></div>
          <div id="role-edit-perms">${permCheckboxesHtml(role.permissions || [])}</div>
          <div id="role-edit-result"></div>
          <div style="margin-top:12px;">
            <button class="btn btn-primary" id="role-save">${t("common.save")}</button>
            <button class="btn btn-ghost" id="role-close">${t("common.cancel")}</button>
          </div>
        </div>`;
      $("role-close").addEventListener("click", () => ($("role-modal").innerHTML = ""));
      $("role-save").addEventListener("click", async () => {
        const description = $("role-edit-desc").value;
        const permissions = readPermCheckboxes($("role-edit-perms"));
        const res = await request(`/api/admin/roles/${encodeURIComponent(name)}`, "PUT", { description, permissions });
        if (res.error) {
          $("role-edit-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(res.error.message)}</div>`;
          return;
        }
        toast(t("roles.saved"));
        render();
      });
    }
  });
}

export { renderRoles };
