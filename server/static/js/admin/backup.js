// Backup / restore section: create, download, restore (from stored backup or
// an uploaded file), and delete backups. Transfers run over the encrypted
// channel in 2 MiB base64 chunks.
//
// The small DOM helpers ($, esc, toast, request, setPanel) are local copies so
// this file is self-contained; `render` comes from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal } from "../modal.js";
import { arrayBufferToBase64, base64ToArrayBuffer } from "../crypto.js";
import { t } from "../i18n.js";
import { render } from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- Backup / restore ------------------------------------------------------
const BACKUP_CHUNK_SIZE = 2 * 1024 * 1024;

function formatBytes(n) {
  if (n == null) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1073741824) return `${(n / 1048576).toFixed(1)} MB`;
  return `${(n / 1073741824).toFixed(2)} GB`;
}

// Download a stored backup over the encrypted channel in 2 MiB base64 chunks,
// reassemble it into a Blob and trigger a browser download.
async function downloadBackup(name) {
  const parts = [];
  let filename = name;
  let index = 0;
  for (;;) {
    const res = await request(`/api/admin/backup/${encodeURIComponent(name)}/download?chunk_index=${index}`, "GET");
    if (res.error) throw new Error(res.error.message);
    const d = res.data || {};
    if (d.filename) filename = d.filename;
    if (d.chunk_data_b64) parts.push(base64ToArrayBuffer(d.chunk_data_b64));
    if (d.is_last_chunk) break;
    index += 1;
  }
  const blob = new Blob(parts, { type: "application/gzip" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

// Upload a backup archive over the encrypted channel in 2 MiB base64 chunks
// (initiate -> chunk* -> finalize); the server reassembles and restores it.
async function uploadBackup(file) {
  const init = await request("/api/admin/backup/restore/initiate", "POST", {});
  if (init.error) throw new Error(init.error.message);
  const upload_id = init.data.upload_id;
  const total = Math.ceil(file.size / BACKUP_CHUNK_SIZE) || 1;
  for (let i = 0; i < total; i++) {
    const buf = await file.slice(i * BACKUP_CHUNK_SIZE, (i + 1) * BACKUP_CHUNK_SIZE).arrayBuffer();
    const r = await request("/api/admin/backup/restore/chunk", "POST", {
      upload_id,
      chunk_index: i,
      chunk_data_b64: arrayBufferToBase64(buf),
    });
    if (r.error) throw new Error(r.error.message);
  }
  const fin = await request("/api/admin/backup/restore/finalize", "POST", {
    upload_id,
    total_chunks: total,
  });
  if (fin.error) throw new Error(fin.error.message);
}

async function renderBackup() {
  const res = await request("/api/admin/backup", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("backup.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const backups = res.data || [];
  const rows = backups
    .map(
      (b) => `
        <tr>
          <td><strong>${esc(b.name)}</strong></td>
          <td>${formatBytes(b.size)}</td>
          <td>${esc(b.created_at || "")}</td>
          <td>${b.include_user_data ? t("common.yes") : t("common.no")}</td>
          <td class="admin-actions">
            <button class="btn btn-sm btn-ghost" data-bact="download" data-name="${esc(b.name)}">${t("backup.download")}</button>
            <button class="btn btn-sm btn-primary" data-bact="restore" data-name="${esc(b.name)}">${t("backup.restore")}</button>
            <button class="btn btn-sm btn-danger" data-bact="delete" data-name="${esc(b.name)}">${t("common.delete")}</button>
          </td>
        </tr>`
    )
    .join("");

  setPanel(`
    <div class="card">
      <h3>${t("backup.createTitle")}</h3>
      <p class="muted">${t("backup.createHelp")}</p>
      <label class="checkbox"><input type="checkbox" id="bk-include-data"> ${t("backup.includeUserData")}</label>
      <div style="margin-top:12px;"><button class="btn btn-primary" id="bk-create">${t("backup.create")}</button></div>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">${t("backup.listTitle")}</h3>
      <table class="admin-table">
        <thead><tr><th>${t("common.name")}</th><th>${t("backup.size")}</th><th>${t("backup.created")}</th><th>${t("backup.userData")}</th><th></th></tr></thead>
        <tbody id="bk-rows">${rows || `<tr><td colspan="5" class="muted">${t("backup.noBackups")}</td></tr>`}</tbody>
      </table>
    </div>
    <div class="card">
      <h3>${t("backup.restoreFromFileTitle")}</h3>
      <p class="muted">${t("backup.restoreFromFileHelp")}</p>
      <input type="file" id="bk-file" accept=".tar.gz,.tgz,application/gzip">
      <div style="margin-top:12px;"><button class="btn btn-primary" id="bk-restore-file">${t("backup.restoreFileButton")}</button></div>
    </div>
  `);

  $("bk-create").addEventListener("click", async () => {
    const include_user_data = $("bk-include-data").checked;
    const r = await request("/api/admin/backup/create", "POST", { include_user_data });
    if (r.error) return toast(r.error.message, "error");
    toast(t("backup.createdToast", { name: r.data.name }));
    render();
  });

  $("bk-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-bact]");
    if (!btn) return;
    const name = btn.dataset.name;
    const act = btn.dataset.bact;
    if (act === "download") {
      try {
        await downloadBackup(name);
        toast(t("backup.downloadStarted"));
      } catch (err) {
        toast(err.message, "error");
      }
      return;
    }
    if (act === "restore") {
      const ok = await confirmModal(
        t("backup.restoreConfirm", { name }),
        { title: t("backup.restoreTitle"), confirmLabel: t("backup.restore"), danger: true }
      );
      if (!ok) return;
      const r = await request("/api/admin/backup/restore", "POST", { name });
      if (r.error) return toast(r.error.message, "error");
      toast(t("backup.restoredToast"));
      render();
      return;
    }
    if (act === "delete") {
      if (!(await confirmModal(t("backup.deleteConfirm", { name }), { title: t("backup.deleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
      const r = await request(`/api/admin/backup/${encodeURIComponent(name)}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast(t("backup.deleted"));
      render();
    }
  });

  $("bk-restore-file").addEventListener("click", async () => {
    const input = $("bk-file");
    if (!input.files || !input.files.length) return toast(t("backup.restoreFileNoFile"), "error");
    const file = input.files[0];
    const ok = await confirmModal(
      t("backup.restoreFileConfirm", { name: file.name }),
      { title: t("backup.restoreFromFileTitle"), confirmLabel: t("backup.restore"), danger: true }
    );
    if (!ok) return;
    try {
      await uploadBackup(file);
      toast(t("backup.restoredToast"));
      render();
    } catch (err) {
      toast(err.message, "error");
    }
  });
}

export { renderBackup };
