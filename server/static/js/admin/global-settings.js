// Global Settings section: the truly global (non-template) settings — just
// the global default GPU. The template schema (the base-layer default for
// every setting) is the "template settings" and is edited on the Templates
// page, not here.
//
// The small DOM helpers ($, esc, toast, request) come from util.js.
import { $, esc, toast, request } from "../util.js";
import { t } from "../i18n.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- Global settings -------------------------------------------------------
async function renderGlobalSettings() {
  const res = await request("/api/admin/global-settings", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("gs.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const data = res.data || {};
  const gpus = data.gpus || [];
  const globalGpu = data.global_default_gpu || "";

  const gpuOptions =
    `<option value="">${t("gs.gpuNone")}</option>` +
    gpus
      .map(
        (g) =>
          `<option value="${esc(g.device)}" ${globalGpu === g.device ? "selected" : ""}>${esc(g.device)}${g.driver ? ` (${esc(g.driver)})` : ""}</option>`
      )
      .join("");

  setPanel(`
    <div class="card">
      <h3>${t("gs.heading")}</h3>
      <p class="muted">${t("gs.help", { templates: t("nav.templates"), groups: t("nav.groups") })}</p>
    </div>
    <div class="card">
      <h3>${t("gs.gpuTitle")}</h3>
      <p class="muted">${t("gs.gpuHelp")}</p>
      <div class="field">
        <label for="gs-gpu-select">${t("common.gpu")}</label>
        <select id="gs-gpu-select">${gpuOptions}</select>
      </div>
      <p id="gs-gpu-error" class="error" role="alert" hidden></p>
      <div style="margin-top:10px;display:flex;gap:8px;flex-wrap:wrap;">
        <button class="btn btn-primary" id="gs-save" type="button">${t("gs.save")}</button>
      </div>
    </div>
  `);

  const saveBtn = $("gs-save");
  if (saveBtn) saveBtn.addEventListener("click", onGlobalSettingsSave);
}

// Save the global (non-template) settings — just the global default GPU.
// The template schema (base-layer defaults) is saved from the Templates page.
async function onGlobalSettingsSave() {
  const errEl = $("gs-gpu-error");
  const saveBtn = $("gs-save");
  if (errEl) errEl.hidden = true;
  if (saveBtn) saveBtn.disabled = true;
  try {
    const res = await request("/api/admin/global-settings", "POST", {
      global_default_gpu: $("gs-gpu-select").value,
    });
    if (res.error) throw new Error(res.error.message || t("gs.saveFailed"));
    toast(t("gs.saved"));
    await renderGlobalSettings();
  } catch (e) {
    if (errEl) { errEl.textContent = e.message; errEl.hidden = false; }
  } finally {
    const b = $("gs-save");
    if (b) b.disabled = false;
  }
}

export { renderGlobalSettings };
