// Accounts section (the unified user/admin roster): account cards, the
// "Add account" form, and the per-account modals (access, home directories,
// set/reset password, edit).
//
// The small DOM helpers ($, esc, toast, request) are local copies so this
// file is self-contained; the cross-section helpers (settings form,
// permission catalog, shared state, render) come from admin.js.
import { $, esc, toast, request } from "../util.js";
import { setUserPassword } from "../auth.js";
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
  pickSectionHtml,
  wirePickSections,
  readPermCheckboxes,
  render,
} from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- User / Admin / Group roster -----------------------------------------
function userCard(user, homedirs, adminGroups = []) {
  const g = user.settings || {};
  const chip = (ok, label) => `<span class="badge ${ok ? "badge-ok" : "badge-warn"}">${label}</span>`;
  const homeText = homedirs && homedirs.length ? homedirs.map(esc).join(", ") : "—";
  const sso = !!user.is_sso;
  // SSO groups: the provider (IdP) groups the user belongs to, captured on
  // their most recent SSO login. Groups that are in the admin-groups list are
  // what make the user an admin, so they are highlighted.
  const ssoGroups = user.sso_groups || [];
  const ssoBadges = ssoGroups
    .map(
      (grp) =>
        `<span class="badge${adminGroups.includes(grp) ? " badge-ok" : ""}" title="${adminGroups.includes(grp) ? t("accounts.ssoGroupAdminTitle") : t("accounts.ssoGroupTitle")}">${t("accounts.ssoGroupBadge", { group: esc(grp) })}${adminGroups.includes(grp) ? t("accounts.ssoGroupAdminSuffix") : ""}</span>`
    )
    .join(" ");
  const isAdmin = !!user.is_admin;
  // SSO users authenticate through the IdP — their password is not local, so
  // they get an SSO badge instead of a Set password control.
  const setPwBtn = sso
    ? ""
    : `<button class="btn btn-sm btn-ghost" data-action="setpw" data-username="${esc(user.username)}">${t("accounts.btnSetPassword")}</button>`;
  const pwChip = sso ? "" : user.has_password ? chip(true, t("accounts.chipPasswordSet")) : `<span class="badge">${t("accounts.chipNoPassword")}</span>`;
  // The unified roster lists everyone; the admin badge marks admins. The
  // is_admin flag is changed from the Edit form (an in-place Admin checkbox),
  // not from a button here. data-username lets the Edit action open its form
  // in this card's place.
  return `
    <div class="card" data-username="${esc(user.username)}">
       <div class="row row-between row-wrap gap-10" >
        <div>
          <strong>${esc(user.username)}</strong>
          ${isAdmin ? `<span class="badge badge-ok ml-8" >${t("accounts.badgeAdmin")}</span>` : ""}
          ${sso ? `<span class="badge ml-8" >${t("nav.sso")}</span>` : ""}
        </div>
        <div class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-action="edit" data-username="${esc(user.username)}">${t("common.edit")}</button>
          <button class="btn btn-sm btn-ghost" data-action="access" data-username="${esc(user.username)}">${t("accounts.btnAccess")}</button>
          ${setPwBtn}
          <button class="btn btn-sm btn-ghost" data-action="homedirs" data-username="${esc(user.username)}">${t("accounts.btnHomeDirs")}</button>
          <button class="btn btn-sm btn-danger" data-action="delete" data-username="${esc(user.username)}">${t("common.delete")}</button>
        </div>
      </div>
       <div class="mt-10 row gap-6 row-wrap" >
        ${chip(g.active, g.active ? t("accounts.chipActive") : t("accounts.chipInactive"))}
        ${pwChip}
        ${chip(g.persistent_storage, g.persistent_storage ? t("accounts.chipStorage") : t("accounts.chipNoStorage"))}
        ${chip(g.public_sharing, g.public_sharing ? t("accounts.chipSharing") : t("accounts.chipNoSharing"))}
        ${chip(g.gpu, g.gpu ? t("accounts.chipGpu") : t("accounts.chipNoGpu"))}
        <span class="badge">${t("accounts.badgeGroup", { group: esc(g.group || t("common.none")) })}</span>
        ${(user.roles || []).length ? `<span class="badge">${t("accounts.badgeRoles", { roles: esc((user.roles || []).join(", ")) })}</span>` : ""}
        <span class="badge">${t("accounts.badgeStorage", { value: g.storage_limit == null || g.storage_limit < 0 ? "∞" : g.storage_limit + "MB" })}</span>
        <span class="badge">${t("accounts.badgeSessions", { value: g.session_limit == null || g.session_limit < 0 ? "∞" : g.session_limit })}</span>
        ${ssoBadges}
      </div>
      <div class="muted mt-8 fs-sm" >${t("accounts.badgeHomeDirs", { homeText })}</div>
    </div>`;
}

async function renderRoster() {
  const res = await fetchData();
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("accounts.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  // The roster is unified: every account (admins and users) appears in one
  // list, admins first. The is_admin flag is what distinguishes them, and it
  // can be flipped in place via the toggle on each card.
  const list = [...res.admins, ...res.users];
  // Fetch the configured SSO admin-groups list so each card can highlight
  // the SSO groups that grant admin (best-effort: a failure just means no
  // highlighting). The list is read-only (sourced from the environment).
  const agRes = await request("/api/admin/sso-groups", "GET");
  const adminGroups = (agRes.data && agRes.data.admin_groups) || [];
  const cards = list.map((u) => userCard(u, u.homedirs, adminGroups)).join("");
  setPanel(`
    <div class="card">
      <h3>${t("accounts.addTitle")}</h3>
      <p class="muted">${t("accounts.addHelp")}</p>
      <div class="admin-form-row mb-12" >
        <div class="field"><label>${t("common.username")}</label><input type="text" id="roster-new-username" autocomplete="off"></div>
        <div class="field"><label class="check"><input type="checkbox" id="roster-new-isadmin"><span>${t("accounts.labelAdmin")}</span></label></div>
      </div>
      ${settingsFormHtml(defaultSettings(lastGroups), lastGroups, "roster-new-settings")}
       <div class="mt-12" ><button class="btn btn-primary" id="roster-create">${t("common.create")}</button></div>
      <div id="roster-result"></div>
    </div>
    <div class="section-title">${t("accounts.heading")}</div>
    <div id="roster-cards">${cards}</div>
    <div id="roster-modal"></div>
  `);
  wirePickSections($("admin-section"));
  wireRoster();
}

async function wireRoster() {
  // The roster is unified: one list of every account, and the `is_admin` flag
  // in the create payload (from the "Admin" checkbox) decides the account kind.
  const basePath = "/api/admin/people";
  const homeBase = "/api/admin/people";

  const createBtn = $("roster-create");
  if (createBtn) {
    createBtn.addEventListener("click", async () => {
      const username = $("roster-new-username").value.trim();
      if (!username) return toast(t("accounts.toastUsernameRequired"), "error");
      const settings = readSettingsForm("roster-new-settings");
      const payload = { username, settings, is_admin: !!$("roster-new-isadmin").checked };
      const res = await request(basePath, "POST", payload);
      const result = $("roster-result");
      if (res.error) {
        result.innerHTML = `<div class="error mt-10" >${esc(res.error.message)}</div>`;
        return;
      }
      toast(t("accounts.toastCreated"));
      setTimeout(render, 300);
    });
  }

  const cards = $("roster-cards");
  if (cards) {
    cards.addEventListener("click", async (e) => {
      const btn = e.target.closest("button[data-action]");
      if (!btn) return;
      const username = btn.dataset.username;
      const action = btn.dataset.action;

      if (action === "delete") {
        if (!(await confirmModal(t("accounts.confirmDelete", { username }), { title: t("accounts.confirmDeleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
        const res = await request(`${basePath}/${encodeURIComponent(username)}`, "DELETE");
        if (res.error) return toast(res.error.message, "error");
        toast(t("accounts.toastDeleted"));
        render();
        return;
      }

      if (action === "homedirs") {
        const res = await request(`${homeBase}/${encodeURIComponent(username)}/homedirs`, "GET");
        if (res.error) return toast(res.error.message, "error");
        const dirs = (res.data && res.data.home_dirs) || [];
        const listHtml = dirs.length
          ? dirs
              .map(
                (d) => `
                <div class="share-row">
                  <code>${esc(d)}</code>
                  <button class="btn btn-sm btn-danger" data-rmhome="${esc(d)}">${t("accounts.btnRemove")}</button>
                </div>`
              )
              .join("")
          : `<p class="muted">${t("accounts.homedirsEmpty")}</p>`;
        $("roster-modal").innerHTML = `
          <div class="card mt-16" >
            <h3>${t("accounts.homedirsTitle", { username: esc(username) })}</h3>
            <div id="home-list">${listHtml}</div>
            <div class="admin-form-row mt-10" >
              <div class="field"><label>${t("accounts.homedirsLabelNew")}</label><input type="text" id="new-home" placeholder="${t("accounts.homedirsPlaceholder")}"></div>
              <button class="btn btn-primary" id="add-home">${t("accounts.btnAdd")}</button>
              <button class="btn btn-ghost" id="close-modal">${t("common.close")}</button>
            </div>
          </div>`;
        $("close-modal").addEventListener("click", () => ($("roster-modal").innerHTML = ""));
        $("add-home").addEventListener("click", async () => {
          const name = $("new-home").value.trim();
          if (!name) return toast(t("accounts.toastNameRequired"), "error");
          const r = await request(`${homeBase}/${encodeURIComponent(username)}/homedirs`, "POST", { home_name: name });
          if (r.error) return toast(r.error.message, "error");
          toast(t("accounts.toastAdded"));
          render();
        });
        $("home-list").addEventListener("click", async (e) => {
          const btn = e.target.closest("button[data-rmhome]");
          if (!btn) return;
          if (!(await confirmModal(t("accounts.confirmRemoveHome"), { confirmLabel: t("accounts.btnRemove"), danger: true }))) return;
          const r = await request(`${homeBase}/${encodeURIComponent(username)}/homedirs/${encodeURIComponent(btn.dataset.rmhome)}`, "DELETE");
          if (r.error) return toast(r.error.message, "error");
          toast(t("accounts.toastRemoved"));
          render();
        });
        return;
      }

      if (action === "access") {
        const d = await fetchData();
        const target = (d.users.concat(d.admins)).find((u) => u.username === username);
        if (!target) return toast(t("accounts.toastNotFound"), "error");
        await loadPermCatalog();
        const roles = target.roles || [];
        const perms = target.permissions || [];
        const groups = target.groups || [];
        // Role options come from the management data (all roles), not the catalog.
        const allRoles = (d.data && d.data.roles) || [];
        const roleChecks = roleCheckboxesHtml(roles, allRoles, "access-roles", t("accounts.accessLabelRoles"));
        const groupRows = lastGroups
          .map(
            (gn) => `<label class="pick-item"><input type="checkbox" name="group" value="${esc(gn)}"${groups.includes(gn) ? " checked" : ""}> <span class="pick-item-name">${esc(gn)}</span></label>`
          )
          .join("");
        const groupChecks = pickSectionHtml("access-groups", t("accounts.accessLabelGroups"), lastGroups.length, groupRows, { empty: t("accounts.accessNoGroups") });
        $("roster-modal").innerHTML = `
          <div class="card mt-16" >
            <h3>${t("accounts.accessTitle", { username: esc(username) })}</h3>
            <p class="muted">${t("accounts.accessHelp")}</p>
            ${roleChecks}
            <div class="field mb-10" ><label>${t("accounts.accessLabelPerms")}</label><div id="access-perms">${permCheckboxesHtml(perms, "perm", "access-perms")}</div></div>
            ${groupChecks}
            <div id="access-result"></div>
             <div class="mt-12" >
              <button class="btn btn-primary" id="access-save">${t("common.save")}</button>
              <button class="btn btn-ghost" id="close-modal">${t("common.cancel")}</button>
            </div>
          </div>`;
        wirePickSections($("roster-modal"));
        $("close-modal").addEventListener("click", () => ($("roster-modal").innerHTML = ""));
        $("access-save").addEventListener("click", async () => {
          const form = $("roster-modal");
          const newRoles = Array.from(form.querySelectorAll('input[name="role"]:checked')).map((el) => el.value);
          const newPerms = readPermCheckboxes($("access-perms"));
          const newGroups = Array.from(form.querySelectorAll('input[name="group"]:checked')).map((el) => el.value);
          const res = await request(`${basePath}/${encodeURIComponent(username)}/access`, "POST", { roles: newRoles, permissions: newPerms, groups: newGroups });
          if (res.error) {
            $("access-result").innerHTML = `<div class="error mt-10" >${esc(res.error.message)}</div>`;
            return;
          }
          toast(t("accounts.toastAccessSaved"));
          render();
        });
        return;
      }

      if (action === "setpw") {
        const d = await fetchData();
        const target = (d.users.concat(d.admins)).find((u) => u.username === username);
        if (!target) return toast(t("accounts.toastNotFound"), "error");
        const hasPw = !!target.has_password;
        $("roster-modal").innerHTML = `
          <div class="card mt-16" >
            <h3>${hasPw ? t("accounts.passwordResetTitle", { username: esc(username) }) : t("accounts.passwordSetTitle", { username: esc(username) })}</h3>
            <p class="muted">${t("accounts.passwordHelp", { username: esc(username) })}</p>
            <div class="admin-form-row">
              <div class="field"><label>${t("accounts.passwordLabelNew")}</label><input type="password" id="setpw-new" minlength="8" autocomplete="new-password" placeholder="${t("accounts.passwordPlaceholder")}"></div>
              <div class="field"><label>${t("accounts.passwordLabelConfirm")}</label><input type="password" id="setpw-confirm" minlength="8" autocomplete="new-password"></div>
            </div>
            <div id="setpw-result"></div>
             <div class="mt-12" >
              <button class="btn btn-primary" id="setpw-save">${t("accounts.btnSavePassword")}</button>
              <button class="btn btn-ghost" id="close-modal">${t("common.cancel")}</button>
            </div>
          </div>`;
        $("close-modal").addEventListener("click", () => ($("roster-modal").innerHTML = ""));
        $("setpw-save").addEventListener("click", async () => {
          const pw = $("setpw-new").value;
          if (pw.length < 8) return toast(t("accounts.toastPasswordTooShort"), "error");
          if (pw !== $("setpw-confirm").value) return toast(t("accounts.toastPasswordMismatch"), "error");
          try {
            await setUserPassword(username, pw);
          } catch (e) {
            $("setpw-result").innerHTML = `<div class="error mt-10" >${esc(e.message)}</div>`;
            return;
          }
          $("roster-modal").innerHTML = "";
          toast(t("accounts.toastPasswordDone", { action: hasPw ? t("accounts.passwordActionReset") : t("accounts.passwordActionSet"), username }));
          render();
        });
        $("setpw-new").focus();
        return;
      }

      if (action === "edit") {
        const d = await fetchData();
        const target = (d.users.concat(d.admins)).find((u) => u.username === username);
        if (!target) return toast(t("accounts.toastNotFound"), "error");
        // Open the edit form in place: it replaces this account's card so the
        // "Edit <name>" form occupies the same space the card did.
        const card = cards.querySelector(`.card[data-username="${username}"]`);
        if (!card) return toast(t("accounts.toastNotFound"), "error");
        // The Admin checkbox lives in the edit form (is_admin is not part of
        // the settings payload). The bootstrap 'admin' account can't be
        // demoted, so its checkbox is locked on.
        const isRootAdmin = username === "admin";
        const adminField = isRootAdmin
          ? `<label class="check" title="${t("accounts.editAdminLockedTitle")}"><input type="checkbox" id="edit-isadmin" checked disabled> ${t("accounts.labelAdmin")}</label>`
          : `<label class="check"><input type="checkbox" id="edit-isadmin" ${target.is_admin ? "checked" : ""}> ${t("accounts.labelAdmin")}</label>`;
        card.innerHTML = `
          <h3>${t("accounts.editTitle", { username: esc(username) })}</h3>
          <div class="admin-form-row mb-12" >
            <div class="field"><label>${t("common.username")}</label><input type="text" value="${esc(username)}" disabled></div>
            <div class="field">${adminField}</div>
          </div>
          ${settingsFormHtml(target.settings || defaultSettings(lastGroups), lastGroups, "edit-settings")}
          <div id="edit-result"></div>
           <div class="mt-12" >
            <button class="btn btn-primary" id="save-edit">${t("common.save")}</button>
            <button class="btn btn-ghost" id="cancel-edit">${t("common.cancel")}</button>
          </div>`;
        wirePickSections(card);
        $("cancel-edit").addEventListener("click", () => render());
        $("save-edit").addEventListener("click", async () => {
          const settings = readSettingsForm("edit-settings");
          const errEl = $("edit-result");
          // If the admin status changed, flip it via the dedicated endpoint
          // first (is_admin is not part of the settings payload).
          const wantAdmin = isRootAdmin ? true : !!$("edit-isadmin").checked;
          if (wantAdmin !== !!target.is_admin) {
            const r = await request(`${basePath}/${encodeURIComponent(username)}/admin-status`, "POST", { is_admin: wantAdmin });
            if (r.error) { errEl.innerHTML = `<div class="error mt-10" >${esc(r.error.message)}</div>`; return; }
          }
          const r = await request(`${basePath}/${encodeURIComponent(username)}`, "PUT", { settings });
          if (r.error) { errEl.innerHTML = `<div class="error mt-10" >${esc(r.error.message)}</div>`; return; }
          toast(t("common.saved"));
          render();
        });
      }
    });
  }
}

export { renderRoster };
