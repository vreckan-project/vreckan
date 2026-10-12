// Feat 13 — self-service home directories. Owns the currentSettings state
// (set by app.js's loadMain); read by the home-directory helpers so the
// Manage modal can refresh the selects.
import { $, toast, esc } from "../util.js";
import { secureFetch } from "../api.js";
import { openCustomModal, confirmModal } from "../modal.js";
import { loadHomedirs, selectHomeDir } from "../files.js";
import { showView } from "./nav.js";
import { currentSettings } from "../app.js";
import { t } from "../i18n.js";

// Populate the launch "Home directory" select: auto + cleanroom always, plus
// the user's persistent home dirs when persistent storage is enabled. The
// current selection is preserved if it still exists.
async function populateHomeSelect() {
  const homeSel = $("opt-home");
  if (!homeSel) return;
  const previous = homeSel.value;
  homeSel.innerHTML = "";
  const addOpt = (value, label) => {
    const opt = document.createElement("option");
    opt.value = value;
    opt.textContent = label;
    homeSel.appendChild(opt);
  };
  addOpt("auto", t("launch.auto"));
  addOpt("cleanroom", t("launch.cleanroom"));
  if (currentSettings.persistent_storage) {
    try {
      const { home_dirs } = await secureFetch("/api/homedirs", { method: "GET" });
      for (const dir of home_dirs || []) {
        if (dir !== "_vreckan_shared_files" && !dir.startsWith("auto-")) {
          addOpt(dir, dir);
        }
      }
    } catch (e) {
      /* persistent storage may be unavailable; auto/cleanroom remain */
    }
  }
  if ([...homeSel.options].some((o) => o.value === previous)) {
    homeSel.value = previous;
  }
}

// Re-fetch home dirs into both the launch select and the Files view.
async function refreshHomeSelects() {
  await populateHomeSelect();
  try {
    await loadHomedirs(true);
  } catch (e) {
    /* non-fatal: the Files view refreshes on its next load */
  }
}

// Self-service modal: list, create, and delete the user's home directories.
function openHomeDirsModal() {
  const { box, close } = openCustomModal({
    title: t("homedirs.title"),
    wide: true,
    body: `
      <p class="muted">${t("homedirs.help")}</p>
      <div id="homedirs-list"></div>
      <hr>
      <div class="field-inline">
        <input type="text" id="homedir-new" placeholder="${t("homedirs.namePlaceholder")}" pattern="[a-zA-Z0-9_-]+">
        <button class="btn btn-primary" id="homedir-create" type="button">${t("common.create")}</button>
      </div>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="homedirs-close">${t("common.close")}</button>
      </div>`,
  });

  const listEl = box.querySelector("#homedirs-list");
  const input = box.querySelector("#homedir-new");
  const createBtn = box.querySelector("#homedir-create");
  box.querySelector('[data-act="homedirs-close"]').addEventListener("click", () => close());

  const refresh = async () => {
    try {
      const { home_dirs } = await secureFetch("/api/homedirs", { method: "GET" });
      const dirs = (home_dirs || []).filter(
        (d) => d !== "_vreckan_shared_files" && !d.startsWith("auto-")
      );
      if (!dirs.length) {
        listEl.innerHTML = `<p class="muted">${t("homedirs.none")}</p>`;
        return;
      }
      listEl.innerHTML = dirs
        .map(
          (d) => `<div class="homedir-row">
            <span>${esc(d)}</span>
            <span class="homedir-row-actions">
              <button class="btn btn-ghost btn-sm" data-manage="${esc(d)}" type="button">${t("common.manage")}</button>
              <button class="btn btn-ghost btn-sm" data-del="${esc(d)}" type="button">${t("common.delete")}</button>
            </span>
          </div>`
        )
        .join("");
      listEl.querySelectorAll("[data-manage]").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const name = btn.getAttribute("data-manage");
          close();
          try {
            await selectHomeDir(name);
            showView("files");
          } catch (e) {
            toast(t("homedirs.couldNotOpen", { name, message: e.message }), "error");
          }
        });
      });
      listEl.querySelectorAll("[data-del]").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const name = btn.getAttribute("data-del");
          const ok = await confirmModal(
            t("homedirs.deleteConfirm", { name }),
            { title: t("homedirs.deleteTitle"), confirmLabel: t("common.delete"), danger: true }
          );
          if (ok) {
            try {
              await secureFetch(`/api/homedirs/${encodeURIComponent(name)}`, { method: "DELETE" });
              toast(t("homedirs.deleted", { name }));
              await refreshHomeSelects();
            } catch (e) {
              toast(t("homedirs.deleteFailed", { message: e.message }), "error");
            }
          }
          // The confirm dialog shares the modal root and replaces this
          // modal's content, so re-open it (refreshed) in either case.
          openHomeDirsModal();
        });
      });
    } catch (e) {
      listEl.innerHTML = `<p class="error">${t("homedirs.loadFailed", { message: esc(e.message) })}</p>`;
    }
  };

  const doCreate = async () => {
    const name = input.value.trim();
    if (!name) {
      toast(t("homedirs.enterName"), "error");
      return;
    }
    if (!/^[a-zA-Z0-9_-]+$/.test(name)) {
      toast(t("homedirs.invalidName"), "error");
      return;
    }
    createBtn.disabled = true;
    try {
      await secureFetch("/api/homedirs", { method: "POST", body: { home_name: name } });
      toast(t("homedirs.created", { name }));
      input.value = "";
      await refresh();
      await refreshHomeSelects();
    } catch (e) {
      toast(t("homedirs.createFailed", { message: e.message }), "error");
    } finally {
      createBtn.disabled = false;
    }
  };

  createBtn.addEventListener("click", doCreate);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doCreate();
  });

  refresh();
}

export { populateHomeSelect, refreshHomeSelects, openHomeDirsModal };
