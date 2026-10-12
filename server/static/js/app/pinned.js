// Feat 7 — pinned launch options ("save these launch options").
// The installed-apps list is owned by app.js (set in loadMain); used here to
// resolve app names in the pinned list and to populate the save-preset app
// dropdown.
import { $, toast, esc } from "../util.js";
import { secureFetch } from "../api.js";
import { openCustomModal, confirmModal } from "../modal.js";
import { installedApps } from "../app.js";
import { t } from "../i18n.js";

// The user's saved presets (set by loadPinned); read by app.js (loadMain).
export let pinnedBehaviors = [];

// Fetch + render the user's pinned presets.
async function loadPinned() {
  try {
    pinnedBehaviors = (await secureFetch("/api/pinned", { method: "GET" })) || [];
  } catch (e) {
    pinnedBehaviors = [];
  }
  renderPinned();
}

function renderPinned() {
  const section = $("pinned-section");
  const list = $("pinned-list");
  if (!section || !list) return;
  if (!pinnedBehaviors.length) {
    section.hidden = true;
    list.innerHTML = "";
    return;
  }
  section.hidden = false;
  const appName = (id) => {
    const app = installedApps.find((a) => a.id === id);
    return app ? app.name : t("pinned.unknownApp");
  };
  const triggerText = (p) => {
    if (p.trigger_type === "all_urls") return t("pinned.allUrls");
    if (p.trigger_type === "file_extension") return `*.${p.trigger_value || "?"}`;
    return t("pinned.manual");
  };
  list.innerHTML = pinnedBehaviors
    .map(
      (p) => `
      <div class="pinned-row">
        <div class="pinned-info">
          <strong>${esc(p.name)}${p.is_default ? ` <span class="default-badge" title="${t("pinned.defaultBadgeTitle")}">★ ${t("pinned.default")}</span>` : ''}</strong>
          <span class="muted">${p.is_default ? t("pinned.allApps") : esc(appName(p.application_id))} · ${esc(triggerText(p))}</span>
        </div>
        <div class="pinned-actions">
          ${p.is_default ? "" : `<button class="btn btn-ghost btn-sm" data-makedefault="${esc(p.id)}" type="button">${t("pinned.makeDefault")}</button>`}
          <button class="btn btn-ghost btn-sm" data-apply="${esc(p.id)}" type="button">${t("common.apply")}</button>
          <button class="btn btn-ghost btn-sm" data-del="${esc(p.id)}" type="button">${t("common.delete")}</button>
        </div>
      </div>`
    )
    .join("");
  list.querySelectorAll("[data-apply]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const p = pinnedBehaviors.find((x) => x.id === btn.getAttribute("data-apply"));
      if (p) applyPreset(p);
    });
  });
  list.querySelectorAll("[data-makedefault]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const id = btn.getAttribute("data-makedefault");
      try {
        await secureFetch(`/api/pinned/${id}/set-default`, { method: "POST" });
        await loadPinned();
        // Reflect the new default in the launch controls immediately.
        const newDefault = pinnedBehaviors.find((x) => x.id === id);
        if (newDefault) applyPreset(newDefault, true);
      } catch (e) {
        toast(t("pinned.setDefaultFailed", { message: e.message }), "error");
      }
    });
  });
  list.querySelectorAll("[data-del]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const p = pinnedBehaviors.find((x) => x.id === btn.getAttribute("data-del"));
      if (!p) return;
      const ok = await confirmModal(t("pinned.deleteConfirm", { name: p.name }), {
        title: t("pinned.deleteTitle"),
        confirmLabel: t("common.delete"),
        danger: true,
      });
      if (!ok) return;
      try {
        await secureFetch(`/api/pinned/${p.id}`, { method: "DELETE" });
        toast(t("pinned.deleted", { name: p.name }));
        await loadPinned();
      } catch (e) {
        toast(t("homedirs.deleteFailed", { message: e.message }), "error");
      }
    });
  });
}

// Apply a preset: fill the launch-option controls with the saved values.
// Options whose saved value no longer exists (e.g. a removed home dir or GPU)
// are skipped so the controls keep a valid value. When `silent` is true the
// values are applied without a toast (used to restore the user's default
// preset on page load).
function applyPreset(p, silent = false) {
  const homeSel = $("opt-home");
  const langSel = $("opt-language");
  const gpuField = $("field-gpu");
  const gpuSel = $("opt-gpu");
  const roomChk = $("opt-room-mode");
  const waylandChk = $("opt-wayland");
  if (homeSel && p.home_name && Array.from(homeSel.options).some((o) => o.value === p.home_name)) {
    homeSel.value = p.home_name;
  }
  if (langSel && p.language && Array.from(langSel.options).some((o) => o.value === p.language)) {
    langSel.value = p.language;
  }
  if (gpuSel && gpuField && !gpuField.hidden && p.selected_gpu && Array.from(gpuSel.options).some((o) => o.value === p.selected_gpu)) {
    gpuSel.value = p.selected_gpu;
  }
  if (roomChk) roomChk.checked = !!p.launch_in_room_mode;
  if (waylandChk) waylandChk.checked = p.wayland_mode !== false;
  if (!silent) toast(t("pinned.applied", { name: p.name }));
}

// Open the "save these launch options" modal (captures the current launch
// options, tied to a chosen app + optional informational trigger).
function openSavePresetModal() {
  if (!installedApps.length) {
    toast(t("pinned.installFirst"), "error");
    return;
  }
  const { box, close } = openCustomModal({
    title: t("pinned.saveTitle"),
    body: `
      <p class="muted">${t("pinned.saveHelp")}</p>
      <div class="field">
        <label for="preset-name">${t("pinned.presetName")}</label>
        <input type="text" id="preset-name" maxlength="128" placeholder="${t("pinned.presetNamePlaceholder")}">
      </div>
      <div class="field">
        <label for="preset-app">${t("files.app")}</label>
        <select id="preset-app">
          ${installedApps.map((a) => `<option value="${esc(a.id)}">${esc(a.name)}</option>`).join("")}
        </select>
      </div>
      <div class="field">
        <label for="preset-trigger">${t("pinned.trigger")}</label>
        <select id="preset-trigger">
          <option value="manual" selected>${t("pinned.triggerManual")}</option>
          <option value="all_urls">${t("pinned.allUrls")}</option>
          <option value="file_extension">${t("pinned.fileExtension")}</option>
        </select>
      </div>
      <div class="field" id="preset-ext-field" hidden>
        <label for="preset-ext">${t("pinned.fileExtension")}</label>
        <input type="text" id="preset-ext" placeholder="${t("pinned.fileExtensionPlaceholder")}">
      </div>
      <div class="field">
        <label class="check"><input type="checkbox" id="preset-default"> ${t("pinned.setAsDefault")}</label>
        <p class="muted" id="preset-default-hint">${t("pinned.defaultHint")}</p>
      </div>
      <p id="preset-error" class="error" role="alert" hidden></p>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="preset-cancel">${t("common.cancel")}</button>
        <button type="button" class="btn btn-primary" data-act="preset-save">${t("common.save")}</button>
      </div>`,
  });
  box.querySelector('[data-act="preset-cancel"]').addEventListener("click", () => close());
  const triggerSel = box.querySelector("#preset-trigger");
  const extField = box.querySelector("#preset-ext-field");
  triggerSel.addEventListener("change", () => {
    extField.hidden = triggerSel.value !== "file_extension";
  });
  box.querySelector('[data-act="preset-save"]').addEventListener("click", async () => {
    const name = box.querySelector("#preset-name").value.trim();
    const appId = box.querySelector("#preset-app").value;
    const triggerType = triggerSel.value;
    const triggerValue =
      triggerSel.value === "file_extension" ? box.querySelector("#preset-ext").value.trim().replace(/^\./, "") : "";
    const errEl = box.querySelector("#preset-error");
    if (!name) {
      errEl.textContent = t("pinned.enterName");
      errEl.hidden = false;
      return;
    }
    if (triggerType === "file_extension" && !triggerValue) {
      errEl.textContent = t("pinned.enterExtension");
      errEl.hidden = false;
      return;
    }
    errEl.hidden = true;
    try {
      await secureFetch("/api/pinned", {
        method: "POST",
        body: {
          name,
          application_id: appId,
          home_name: $("opt-home").value || null,
          language: $("opt-language").value,
          selected_gpu: $("field-gpu").hidden ? null : $("opt-gpu").value === "none" ? null : $("opt-gpu").value,
          launch_in_room_mode: $("opt-room-mode").checked,
          wayland_mode: $("opt-wayland").checked,
          trigger_type: triggerType,
          trigger_value: triggerValue,
          is_default: $("preset-default").checked,
        },
      });
      toast(t("pinned.saved", { name }));
      close();
      await loadPinned();
    } catch (e) {
      errEl.textContent = e.message;
      errEl.hidden = false;
    }
  });
  box.querySelector("#preset-name").focus();
}

export { loadPinned, renderPinned, applyPreset, openSavePresetModal };
