// Overview section: server status card + "My settings" form + the
// "How to Use Vreckan" guide modal.
//
// Self-contained on purpose: the small helpers it needs ($, esc, request,
// setPanel) are local copies so this file has no dependency on the other
// admin modules.
import { $, esc, request } from "../util.js";
import { openCustomModal, closeModal } from "../modal.js";
import { t } from "../i18n.js";
// Shared with the rest of the admin UI (defined in admin.js). Referenced only
// inside renderOverview, so the admin.js <-> overview.js import cycle is safe.
import { settingsFormHtml, lastGroups } from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- How to Use guide -----------------------------------------------------
const HOWTO_ITEMS = [
  ["howto.item1Lead", "howto.item1Body"],
  ["howto.item2Lead", "howto.item2Body"],
  ["howto.item3Lead", "howto.item3Body"],
  ["howto.item4Lead", "howto.item4Body"],
  ["howto.item5Lead", "howto.item5Body"],
];

function howToGuideHtml() {
  const items = HOWTO_ITEMS.map(
    ([leadKey, bodyKey]) => `<li><strong>${esc(t(leadKey))}:</strong> ${t(bodyKey)}</li>`
  ).join("");
  return `
    <p class="muted">${t("howto.intro")}</p>
    <ul class="howto-list">${items}</ul>
    <div class="modal-actions">
      <button type="button" class="btn btn-primary" data-act="howto-close">${t("howto.close")}</button>
    </div>`;
}

function openHowToGuide() {
  const { box } = openCustomModal({ title: t("howto.title"), body: howToGuideHtml() });
  const btn = box.querySelector('[data-act="howto-close"]');
  if (btn) btn.addEventListener("click", () => closeModal());
}

// --- Overview -------------------------------------------------------------
async function renderOverview() {
  const [status, data] = await Promise.all([
    request("/api/admin/status", "POST", {}),
    request("/api/admin/data", "POST", {}),
  ]);

  const s = (status.data && status.data.settings) || {};
  const gpus = (status.data && status.data.gpus) || [];
  const kv = [
    [t("overview.kvUsername"), status.data && status.data.username],
    [t("overview.kvIsAdmin"), status.data && status.data.is_admin ? t("overview.yes") : t("overview.no")],
    [t("overview.kvCpu"), status.data && status.data.cpu_model],
    [t("overview.kvDiskTotal"), status.data && status.data.disk_total != null ? `${(status.data.disk_total / 1048576).toFixed(1)} MB` : "—"],
    [t("overview.kvDiskUsed"), status.data && status.data.disk_used != null ? `${(status.data.disk_used / 1048576).toFixed(1)} MB` : "—"],
    [t("overview.kvGpus"), gpus.length ? gpus.map((g) => g.device || "GPU").join(", ") : t("overview.none")],
    [t("overview.kvApiPort"), data.data && data.data.api_port],
    [t("overview.kvSessionPort"), data.data && data.data.session_port],
  ];

  const kvHtml = kv
    .map(
      ([label, value]) => `
      <div class="kv-item">
        <div class="kv-label">${esc(label)}</div>
        <div class="kv-value">${value == null || value === "" ? '<span class="muted">—</span>' : esc(String(value))}</div>
      </div>`
    )
    .join("");

  const errors = [status.error, data.error].filter(Boolean);
  let errorHtml = "";
  if (errors.length) {
    errorHtml = `<div class="error">${errors.map((e) => esc(e.message)).join(" · ")}</div>`;
  }

  setPanel(`
    <div class="card">
       <div class="row row-between row-wrap gap-10" >
        <h3>${t("overview.title")}</h3>
        <button class="btn btn-ghost" id="howto-btn" type="button">${t("overview.howToBtn")}</button>
      </div>
      <p class="muted">${t("overview.description")}</p>
      ${errorHtml}
      <div class="kv-grid mt-14" >${kvHtml}</div>
    </div>
    <div class="card">
      <h3>${t("overview.mySettings")}</h3>
      ${settingsFormHtml(s, lastGroups, "overview-settings")}
    </div>
  `);

  const howtoBtn = $("howto-btn");
  if (howtoBtn) howtoBtn.addEventListener("click", () => openHowToGuide());
}

export { renderOverview };
