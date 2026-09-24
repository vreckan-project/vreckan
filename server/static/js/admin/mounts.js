// Volume mounts section: bind directories from the Docker host into app
// containers. Container paths are relative to the user's home directory.
//
// The small DOM helpers ($, esc, toast, request) come from util.js; the
// cross-section helpers come from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal } from "../modal.js";
import { t } from "../i18n.js";
import {
  lastGroups,
  fetchData,
  render,
} from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- Volume mounts --------------------------------------------------------
async function renderVolumeMounts() {
  const res = await fetchData();
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("mounts.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const mounts = res.volumeMounts || [];
  const userNames = [...(res.admins || []).map((u) => u.username), ...(res.users || []).map((u) => u.username)];
  const groupNames = lastGroups;

  const rows = mounts
    .map((m) => {
      const assign = m.scope === "none" ? "\u2014" : `${m.scope}: ${m.target || "\u2014"}`;
      return `
        <tr>
          <td><strong>${esc(m.name)}</strong></td>
          <td><code>${esc(m.host_path)}</code></td>
          <td><code>${esc(m.container_path)}</code></td>
          <td>${m.read_only ? "ro" : "rw"}</td>
          <td>${esc(assign)}</td>
          <td class="admin-actions">
            <button class="btn btn-sm btn-ghost" data-act="edit" data-name="${esc(m.name)}">${t("common.edit")}</button>
            <button class="btn btn-sm btn-danger" data-act="delete" data-name="${esc(m.name)}">${t("common.delete")}</button>
          </td>
        </tr>`;
    })
    .join("");

  const targetOptions = (scope, selected) => {
    if (scope === "none") return `<option value="">\u2014</option>`;
    const names = scope === "group" ? groupNames : userNames;
    return names
      .map((n) => `<option value="${esc(n)}" ${n === selected ? "selected" : ""}>${esc(n)}</option>`)
      .join("");
  };

  const mountFormHtml = (m, prefix, isNew) => `
    <div class="admin-form-row">
      <div class="field"><label>${t("common.name")}</label><input type="text" id="${prefix}-name" value="${esc(m.name || "")}" placeholder="^[a-zA-Z0-9_-]+$" ${isNew ? "" : "readonly"}></div>
      <div class="field"><label>${t("mounts.hostPath")}</label><input type="text" id="${prefix}-host" value="${esc(m.host_path || "")}" placeholder="/mnt/NVME/apps/depot"></div>
      <div class="field"><label>${t("mounts.containerPath")}</label><input type="text" id="${prefix}-container" value="${esc(m.container_path || "")}" placeholder="depot (relative to home dir)"></div>
    </div>
    <div class="admin-form-row">
      <div class="field"><label>${t("mounts.access")}</label><label class="checkbox"><input type="checkbox" id="${prefix}-ro" ${m.read_only ? "checked" : ""}> ${t("mounts.readOnly")}</label></div>
      <div class="field"><label>${t("mounts.assignTo")}</label><select id="${prefix}-scope">
        <option value="none" ${(m.scope || "none") === "none" ? "selected" : ""}>${t("mounts.scopeNone")}</option>
        <option value="user" ${m.scope === "user" ? "selected" : ""}>${t("mounts.scopeUser")}</option>
        <option value="group" ${m.scope === "group" ? "selected" : ""}>${t("mounts.scopeGroup")}</option>
      </select></div>
      <div class="field"><label>${t("mounts.target")}</label><select id="${prefix}-target" ${(m.scope || "none") === "none" ? "disabled" : ""}>${targetOptions(m.scope || "none", m.target)}</select></div>
    </div>`;

  setPanel(`
    <div class="card">
      <h3>${t("mounts.addTitle")}</h3>
      <p class="muted">${t("mounts.addHelp")}</p>
      ${mountFormHtml({}, "vm-new", true)}
      <div style="margin-top:12px;"><button class="btn btn-primary" id="vm-create">${t("common.create")}</button></div>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">${t("mounts.heading")}</h3>
      <table class="admin-table">
        <thead><tr><th>${t("common.name")}</th><th>${t("mounts.hostPath")}</th><th>${t("mounts.containerPath")}</th><th>${t("mounts.mode")}</th><th>${t("mounts.assignedTo")}</th><th></th></tr></thead>
        <tbody id="vm-rows">${rows || `<tr><td colspan="6" class="muted">${t("mounts.empty")}</td></tr>`}</tbody>
      </table>
    </div>
    <div id="vm-modal"></div>
  `);

  const wireScope = (prefix) => {
    const scopeSel = $(`${prefix}-scope`);
    const targetSel = $(`${prefix}-target`);
    if (!scopeSel || !targetSel) return;
    scopeSel.addEventListener("change", () => {
      const scope = scopeSel.value;
      targetSel.innerHTML = targetOptions(scope, "");
      targetSel.disabled = scope === "none";
    });
  };
  wireScope("vm-new");

  $("vm-create").addEventListener("click", async () => {
    const name = $("vm-new-name").value.trim();
    const host_path = $("vm-new-host").value.trim();
    const container_path = $("vm-new-container").value.trim();
    const read_only = $("vm-new-ro").checked;
    const scope = $("vm-new-scope").value;
    const target = $("vm-new-target").value;
    if (!name) return toast(t("mounts.nameRequired"), "error");
    if (!host_path) return toast(t("mounts.hostPathRequired"), "error");
    if (!container_path) return toast(t("mounts.containerPathRequired"), "error");
    if (scope !== "none" && !target) return toast(t("mounts.pickTarget"), "error");
    const r = await request("/api/admin/volume_mounts", "POST", { name, host_path, container_path, read_only, scope, target });
    if (r.error) return toast(r.error.message, "error");
    toast(t("mounts.created"));
    render();
  });

  $("vm-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const name = btn.dataset.name;
    if (btn.dataset.act === "delete") {
      if (!(await confirmModal(t("mounts.confirmDelete", { name }), { title: t("mounts.confirmDeleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
      const r = await request(`/api/admin/volume_mounts/${encodeURIComponent(name)}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast(t("mounts.deleted"));
      render();
      return;
    }
    if (btn.dataset.act === "edit") {
      const d = await fetchData();
      const m = (d.volumeMounts || []).find((x) => x.name === name);
      if (!m) return;
      $("vm-modal").innerHTML = `
        <div class="card" style="margin-top:16px;">
          <h3>${t("mounts.editTitle", { name: esc(name) })}</h3>
          ${mountFormHtml(m, "vm-edit", false)}
          <div style="margin-top:12px;">
            <button class="btn btn-primary" id="vm-save">${t("common.save")}</button>
            <button class="btn btn-ghost" id="vm-close">${t("common.cancel")}</button>
          </div>
        </div>`;
      wireScope("vm-edit");
      $("vm-close").addEventListener("click", () => ($("vm-modal").innerHTML = ""));
      $("vm-save").addEventListener("click", async () => {
        const r = await request(`/api/admin/volume_mounts/${encodeURIComponent(name)}`, "PUT", {
          host_path: $("vm-edit-host").value.trim(),
          container_path: $("vm-edit-container").value.trim(),
          read_only: $("vm-edit-ro").checked,
          scope: $("vm-edit-scope").value,
          target: $("vm-edit-target").value,
        });
        if (r.error) return toast(r.error.message, "error");
        toast(t("common.saved"));
        render();
      });
    }
  });
}

export { renderVolumeMounts };
