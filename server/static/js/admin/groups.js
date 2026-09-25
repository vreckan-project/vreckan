// Groups section: the group roster (cards), the "Add group" form, and the
// in-place edit form. SSO-observed groups are badged; groups in the admin SSO
// list are highlighted.
//
// The small DOM helpers ($, esc, toast, request) come from util.js; the
// cross-section helpers come from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal } from "../modal.js";
import { t } from "../i18n.js";
import {
  lastGroups,
  fetchData,
  settingsFormHtml,
  defaultSettings,
  readSettingsForm,
  loadPermCatalog,
  permCheckboxesHtml,
  roleCheckboxesHtml,
  render,
} from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- Groups ---------------------------------------------------------------
async function renderGroups() {
  const res = await fetchData();
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("groups.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  // The SSO group configuration lives on this page. It is read-only: the
  // admin/user group lists come from the environment (VRECKAN_OIDC_*_GROUPS)
  // and the observed list is derived from the SSO accounts.
  const ssoRes = await request("/api/admin/sso-groups", "GET");
  const ssoData = ssoRes.data || {};
  const adminGroups = ssoData.admin_groups || [];
  // The Add-group form renders role + permission checkboxes, so make sure the
  // permission catalog is loaded before the panel HTML is built (otherwise the
  // permission list renders empty).
  await loadPermCatalog();
  const allRoles = res.data.roles || [];
  const observedGroups = ssoData.observed || [];
  // Each group is a card (like the account cards) that expands in place when
  // edited. SSO-observed groups are badged so an admin can tell them apart
  // from manually created ones; ones in the admin SSO list are highlighted.
  const cards = res.groups
    .map((g) => {
      const s = g.settings || {};
      const isSso = observedGroups.includes(g.name);
      const ssoBadge = isSso
        ? `<span class="badge${adminGroups.includes(g.name) ? " badge-ok" : ""}" title="${adminGroups.includes(g.name) ? t("groups.ssoAdminTitle") : t("groups.ssoObservedTitle")}">${t("groups.ssoBadge")}${adminGroups.includes(g.name) ? t("groups.ssoAdminSuffix") : ""}</span>`
        : "";
      return `
        <div class="card" data-group-name="${esc(g.name)}">
           <div class="row row-between row-wrap gap-10" >
            <div>
              <strong>${esc(g.name)}</strong>
              ${ssoBadge}
            </div>
            <div class="admin-actions">
              <button class="btn btn-sm btn-ghost" data-act="edit" data-name="${esc(g.name)}">${t("common.edit")}</button>
              <button class="btn btn-sm btn-danger" data-act="delete" data-name="${esc(g.name)}">${t("common.delete")}</button>
            </div>
          </div>
           <div class="mt-10 row gap-6 row-wrap" >
            <span class="badge ${s.active ? "badge-ok" : "badge-warn"}">${s.active ? t("accounts.chipActive") : t("accounts.chipInactive")}</span>
            <span class="badge">${s.persistent_storage ? t("accounts.chipStorage") : t("accounts.chipNoStorage")}</span>
            <span class="badge">${s.gpu ? t("accounts.chipGpu") : t("accounts.chipNoGpu")}</span>
            ${(g.roles || []).length ? `<span class="badge">${t("accounts.badgeRoles", { roles: esc((g.roles || []).join(", ")) })}</span>` : `<span class="badge">${t("groups.noRoles")}</span>`}
            ${(g.permissions || []).length ? `<span class="badge">${t("groups.badgePerms", { perms: esc((g.permissions || []).join(", ")) })}</span>` : ""}
            <span class="badge">${t("accounts.badgeStorage", { value: s.storage_limit == null || s.storage_limit < 0 ? "∞" : s.storage_limit + "MB" })}</span>
          </div>
        </div>`;
    })
    .join("");

  setPanel(`
    <div class="card">
      <h3>${t("groups.addTitle")}</h3>
      <div class="admin-form-row mb-12" >
        <div class="field"><label>${t("common.name")}</label><input type="text" id="group-new-name" placeholder="^[a-zA-Z0-9_-]+$"></div>
      </div>
      ${settingsFormHtml(defaultSettings(lastGroups), lastGroups, "group-new-settings", false)}
      <div class="field mt-12" ><label>${t("groups.rolesLabel")}</label><div id="group-new-roles">${roleCheckboxesHtml([], allRoles)}</div></div>
      <div class="field mt-12" ><label>${t("groups.permsLabel")}</label><div id="group-new-perms">${permCheckboxesHtml([], "perm")}</div></div>
       <div class="mt-12" ><button class="btn btn-primary" id="group-create">${t("common.create")}</button></div>
    </div>
    <div class="section-title">${t("groups.heading")}</div>
    <div id="group-cards">${cards || `<div class="card"><p class="muted">${t("groups.empty")}</p>`}</div>
  `);

  $("group-create").addEventListener("click", async () => {
    const name = $("group-new-name").value.trim();
    if (!name) return toast(t("accounts.toastNameRequired"), "error");
    const settings = readSettingsForm("group-new-settings");
    const roles = Array.from($("group-new-roles").querySelectorAll('input[name="role"]:checked')).map((el) => el.value);
    const permissions = Array.from($("group-new-perms").querySelectorAll('input[name="perm"]:checked')).map((el) => el.value);
    const r = await request("/api/admin/groups", "POST", { name, settings, roles, permissions });
    if (r.error) return toast(r.error.message, "error");
    toast(t("accounts.toastCreated"));
    render();
  });

  const cardsEl = $("group-cards");
  cardsEl.addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const name = btn.dataset.name;
    if (btn.dataset.act === "delete") {
      if (!(await confirmModal(t("groups.confirmDelete", { name }), { title: t("groups.confirmDeleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
      const r = await request(`/api/admin/groups/${encodeURIComponent(name)}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast(t("accounts.toastDeleted"));
      render();
      return;
    }
    if (btn.dataset.act === "edit") {
      const d = await fetchData();
      const g = d.groups.find((x) => x.name === name);
      if (!g) return toast(t("accounts.toastNotFound"), "error");
      // Load the permission catalog before rendering the edit form so the
      // permission checkboxes are populated (the roles list comes from d.data).
      await loadPermCatalog();
      const allRoles = (d.data && d.data.roles) || [];
      // Expand the edit form in place: it replaces this group's card so the
      // form occupies the same space the card did (mirrors the account cards).
      const card = cardsEl.querySelector(`.card[data-group-name="${name}"]`);
      if (!card) return toast(t("accounts.toastNotFound"), "error");
      card.innerHTML = `
        <h3>${t("groups.editTitle", { name: esc(name) })}</h3>
        ${settingsFormHtml(g.settings || defaultSettings(lastGroups), lastGroups, "group-edit-settings", false)}
        <div class="field mt-12" ><label>${t("groups.rolesLabel")}</label><div id="group-edit-roles">${roleCheckboxesHtml(g.roles || [], allRoles)}</div></div>
        <div class="field mt-12" ><label>${t("groups.permsLabel")}</label><div id="group-edit-perms">${permCheckboxesHtml(g.permissions || [], "perm")}</div></div>
        <div id="group-edit-result"></div>
         <div class="mt-12" >
          <button class="btn btn-primary" id="group-save">${t("common.save")}</button>
          <button class="btn btn-ghost" id="group-close">${t("common.cancel")}</button>
        </div>`;
      $("group-close").addEventListener("click", () => render());
      $("group-save").addEventListener("click", async () => {
        const settings = readSettingsForm("group-edit-settings");
        const roles = Array.from($("group-edit-roles").querySelectorAll('input[name="role"]:checked')).map((el) => el.value);
        const permissions = Array.from($("group-edit-perms").querySelectorAll('input[name="perm"]:checked')).map((el) => el.value);
        const r = await request(`/api/admin/groups/${encodeURIComponent(name)}`, "PUT", { settings, roles, permissions });
        if (r.error) {
          $("group-edit-result").innerHTML = `<div class="error mt-10" >${esc(r.error.message)}</div>`;
          return;
        }
        toast(t("common.saved"));
        render();
      });
    }
  });
}

export { renderGroups };
