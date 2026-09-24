// Vreckan web client — wires index.html to auth.js + api.js (E2EE).
// Feature code lives in ./app/*.js (nav, sessions, launch, homedirs, pinned);
// this file is the hub: shared state, main data load, auth flow, and init.
import { login, logout, me, changePassword } from "./auth.js";
import { secureFetch } from "./api.js";
import { resetSession, ensureSession } from "./crypto.js";
import { $, toast, formatLogoSrc } from "./util.js";
import { initFilesView, showFilesTab, loadHomedirs } from "./files.js";
import { initAdminView, showAdminTab, refreshAdminSection } from "./admin.js";
import { loadTranslator, applyTranslations, setLanguage, t, activeLanguage, SUPPORTED_UI_LANGS, resolveLanguage } from "./i18n.js";
import { showView, activateView, refreshMainLists } from "./app/nav.js";
import { renderSessions, loadSessions } from "./app/sessions.js";
import { populateLanguageDropdown, populateGpuDropdown, handleAppSelection, launchModalApp, syncLaunchFileFields, handleLaunchConfirm, pickFileForStorageUpload } from "./app/launch.js";
import { populateHomeSelect, openHomeDirsModal } from "./app/homedirs.js";
import { loadPinned, applyPreset, openSavePresetModal, pinnedBehaviors } from "./app/pinned.js";

// ---------------------------------------------------------------------------
// Shared app state. Each variable is owned by the module that reassigns it
// (so a reassignment never hits another module's temporal-dead-zone binding
// across an import cycle); the other modules import it as a live binding.
//   currentUser, availableGpus, currentSettings, installedApps — here
//   currentAdminSection — nav.js (reassigned by activateView)
//   pinnedBehaviors — pinned.js (reassigned by loadPinned)
// ---------------------------------------------------------------------------
export let currentUser = null;
export let availableGpus = [];
// The current user's effective settings (set in loadMain); read by the
// home-directory helpers (homedirs.js) and the launch code (launch.js).
export let currentSettings = {};
// Installed apps (set in loadMain); read by pinned.js and nav.js.
export let installedApps = [];
export function setInstalledApps(apps) {
  installedApps = Array.isArray(apps) ? apps : [];
}

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------
function showError(el, message) {
  el.textContent = message;
  el.hidden = false;
}
function clearError(el) {
  el.textContent = "";
  el.hidden = true;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
function renderAppCards(apps) {
  const grid = $("app-grid");
  grid.innerHTML = "";
  if (!apps.length) {
    grid.innerHTML = '<p class="muted">No apps installed.</p>';
    return;
  }
  for (const app of apps) {
    const card = document.createElement("div");
    card.className = "app-tile";
    card.dataset.appid = app.id;

    const img = document.createElement("img");
    img.alt = `${app.name} logo`;
    img.src = "/img/icon128.png";
    card.appendChild(img);
    formatLogoSrc(app).then((src) => {
      img.src = src;
    });

    const name = document.createElement("div");
    name.className = "app-name";
    name.textContent = app.name;
    card.appendChild(name);

    const meta = document.createElement("div");
    meta.className = "app-meta";
    if (app.is_meta_app) {
      const b = document.createElement("span");
      b.className = "badge lab";
      b.textContent = "Laboratory";
      meta.appendChild(b);
    }
    if (app.nvidia_support || app.dri3_support) {
      const b = document.createElement("span");
      b.className = "badge nvidia";
      b.textContent = "GPU";
      meta.appendChild(b);
    }
    if (app.url_support) {
      const b = document.createElement("span");
      b.className = "badge url";
      b.textContent = "URL";
      meta.appendChild(b);
    }
    if (app.home_directories) {
      const b = document.createElement("span");
      b.className = "badge files";
      b.textContent = "Files";
      meta.appendChild(b);
    }
    if (meta.children.length) card.appendChild(meta);

    card.addEventListener("click", () => handleAppSelection(app));
    grid.appendChild(card);
  }
}

// ---------------------------------------------------------------------------
// Main data load (mirrors popup.js parallel load)
// ---------------------------------------------------------------------------
async function loadMain() {
  const [statusData, appsData, sessionsData] = await Promise.all([
    secureFetch("/api/admin/status", { method: "POST", body: {} }),
    secureFetch("/api/applications", { method: "POST", body: {} }),
    secureFetch("/api/sessions", { method: "GET" }),
  ]);

  const settings = (statusData && statusData.settings) || {};
  currentSettings = settings;
  availableGpus = (statusData && statusData.gpus) || [];
  setInstalledApps(appsData);

  $("user-label").textContent = statusData.username || currentUser.username;

  // Home directory dropdown: auto + cleanroom always; persistent dirs if enabled.
  await populateHomeSelect();
  // Self-service "Manage" button: only shown when the user has persistent storage.
  const manageBtn = $("btn-manage-homedirs");
  if (manageBtn) manageBtn.hidden = !settings.persistent_storage;
  // Upload-to-storage button (Feat 10): only meaningful with persistent storage.
  const uploadStorageBtn = $("btn-upload-storage");
  if (uploadStorageBtn) uploadStorageBtn.hidden = !settings.persistent_storage;

  populateGpuDropdown();
  populateLanguageDropdown();
  renderAppApps(appsData);
  renderSessions(Array.isArray(sessionsData) ? sessionsData : []);
  // Pinned launch options (Feat 7).
  await loadPinned();

  // Restore the user's default preset (if any) so their saved launch options
  // (GPU, home directory, language, …) are remembered between sessions and are
  // used across every app, not just the one the preset was originally saved for.
  const defaultPreset = pinnedBehaviors.find((p) => p.is_default);
  if (defaultPreset) {
    applyPreset(defaultPreset, true);
  } else if (
    statusData.global_default_gpu &&
    !$("field-gpu").hidden &&
    availableGpus.some((g) => g.device === statusData.global_default_gpu)
  ) {
    // No personal default preset — fall back to the admin's global default GPU.
    $("opt-gpu").value = statusData.global_default_gpu;
  }

  // File manager tab: only available when persistent storage is enabled.
  if (settings.persistent_storage) {
    showFilesTab(true);
    try {
      await loadHomedirs();
    } catch (e) {
      toast(`Failed to load home directories: ${e.message}`, "error");
    }
  } else {
    showFilesTab(false);
  }

  // Admin console: visible when the user holds at least one admin permission
  // (the "admin" super-permission expands to all of them). Each individual
  // section is then shown/hidden by its own admin.<section> permission.
  showAdminTab(
    !!statusData && showAdminTab.hasAnyAdminPermission(statusData.permissions),
    statusData && statusData.permissions
  );
}

function renderAppApps(apps) {
  renderAppCards(Array.isArray(apps) ? apps : []);
}
export { renderAppApps };

// ---------------------------------------------------------------------------
// Auth flow
// ---------------------------------------------------------------------------
async function enterMain() {
  try {
    await ensureSession();
  } catch (e) {
    toast(`Secure channel setup failed: ${e.message}`, "error");
    return false;
  }
  showView("main");
  // The language selector only makes sense once authenticated (the choice is
  // persisted to the account). Reveal it here so it appears after both a fresh
  // form login and a session restore.
  const langWrap = $("lang-select-wrap");
  if (langWrap) langWrap.hidden = false;
  try {
    await loadMain();
  } catch (e) {
    toast(`Failed to load: ${e.message}`, "error");
  }
  // loadMain() has now revealed the Files / admin sidebar entries the user is
  // allowed to see, so it is safe to restore the page they were on before the
  // refresh (see restoreLastView). A fresh login has no stored view and stays
  // on Apps.
  restoreLastView();
  return true;
}

// Remember the page the user is on so a browser refresh lands back on it
// instead of always bouncing to Apps. Stored per browser in localStorage as
// { view, adminSection } and cleared on logout. On restore we only honour a
// view the user is actually allowed to see (Files needs persistent storage,
// admin sections need the admin role) and fall back to Apps otherwise.
function restoreLastView() {
  let stored = null;
  try {
    stored = JSON.parse(localStorage.getItem("vreckan.lastView"));
  } catch (e) {
    /* corrupt / unavailable storage — just stay on Apps */
  }
  if (!stored || typeof stored.view !== "string") return;
  // The unified roster was renamed from "people" to "accounts"; remap any
  // stored value from the old name so a refresh still lands on the right page.
  if (stored.adminSection === "people") stored.adminSection = "accounts";
  const hasAnyAdmin = showAdminTab.hasAnyAdminPermission(
    currentUser?.permissions
  );
  if (stored.view === "admin") {
    if (!hasAnyAdmin) return;
    // Only restore a section the user is actually permitted to see (its
    // sidebar link is not hidden); otherwise fall back to Overview.
    const section =
      stored.adminSection &&
      $("sidebar-nav").querySelector(
        `.nav-link[data-admin-section="${stored.adminSection}"]:not([hidden])`
      )
        ? stored.adminSection
        : "overview";
    activateView("admin", section);
  } else if (stored.view === "files") {
    if ($("tab-files").hidden) return;
    activateView("files");
  }
  // "main" (or anything else) is already the default view.
}

async function checkSso() {
  // Reveal the SSO button only when the server reports OIDC is enabled.
  try {
    const res = await fetch("/api/auth/oidc/status", { credentials: "include" });
    if (!res.ok) return;
    const data = await res.json();
    if (data && data.enabled) {
      const divider = $("login-sso-divider");
      const btn = $("login-sso");
      if (divider) divider.hidden = false;
      if (btn) btn.hidden = false;
    }
  } catch (e) {
    /* network error — leave SSO hidden */
  }
}

async function init() {
  // i18n: resolve the initial language. A language saved on the account
  // (persisted in the DB via the Apply button) wins; otherwise fall back to a
  // per-browser localStorage preference, then the browser language (English
  // fallback). The DB value is only known once we can authenticate, so we try
  // to restore the session first and re-apply if it differs from the initial.
  let initialLang = navigator.language;
  try {
    const saved = localStorage.getItem("vreckan_ui_lang");
    if (saved) initialLang = saved;
  } catch (_) {}
  await loadTranslator(initialLang);
  document.documentElement.lang = resolveLanguage(initialLang);
  applyTranslations(document.body);

  const langSelect = $("lang-select");
  const langWrap = $("lang-select-wrap");
  const langApply = $("lang-apply");
  if (langSelect) {
    langSelect.innerHTML = SUPPORTED_UI_LANGS.map(
      (l) => `<option value="${l.code}">${l.label}</option>`
    ).join("");
    langSelect.value = resolveLanguage(initialLang);
    // Changing the dropdown previews the language immediately; the Apply
    // button persists it to the account (DB) so it follows the user across
    // browsers/devices.
    langSelect.addEventListener("change", async () => {
      try {
        localStorage.setItem("vreckan_ui_lang", langSelect.value);
      } catch (_) {}
      await setLanguage(langSelect.value);
      try {
        if (currentUser) {
          await refreshMainLists();
          await loadPinned();
          if (currentUser.is_admin) refreshAdminSection();
        }
      } catch (_) {}
    });
    if (langApply) {
      langApply.addEventListener("click", () => persistUiLanguage(langSelect.value));
    }
  }

  // Wire login form
  $("login-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    clearError($("login-error"));
    const btn = $("login-submit");
    btn.disabled = true;
    try {
      currentUser = await login(
        $("login-username").value.trim(),
        $("login-password").value
      );
      $("login-password").value = "";
      if (!(await enterMain())) return;
    } catch (err) {
      showError($("login-error"), err.message);
    } finally {
      btn.disabled = false;
    }
  });

  // SSO / OIDC sign-in (only shown when the server has OIDC enabled)
  const ssoBtn = $("login-sso");
  if (ssoBtn) {
    ssoBtn.addEventListener("click", () => {
      ssoBtn.disabled = true;
      ssoBtn.textContent = t("login.redirecting");
      window.location.href = "/api/auth/oidc/authorize";
    });
    checkSso();
  }

  // Logout
  $("btn-logout").addEventListener("click", async () => {
    try {
      await logout();
    } catch (e) {
      /* session may already be gone */
    }
    resetSession();
    try {
      localStorage.removeItem("vreckan.lastView");
    } catch (e) {
      /* non-fatal */
    }
    showView("login");
    toast("Logged out");
  });

  // Change password modal
  $("btn-change-password").addEventListener("click", () => {
    clearError($("cp-error"));
    $("modal-password").hidden = false;
    $("cp-old").focus();
  });
  $("cp-cancel").addEventListener("click", () => {
    $("modal-password").hidden = true;
  });
  $("change-password-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    clearError($("cp-error"));
    const oldPw = $("cp-old").value;
    const newPw = $("cp-new").value;
    const confirm = $("cp-confirm").value;
    if (newPw.length < 8) return showError($("cp-error"), "New password must be at least 8 characters.");
    if (newPw !== confirm) return showError($("cp-error"), "New passwords do not match.");
    try {
      await changePassword(oldPw, newPw);
      $("modal-password").hidden = true;
      $("change-password-form").reset();
      toast("Password updated");
    } catch (err) {
      showError($("cp-error"), err.message);
    }
  });

  // File manager view (listens exist regardless of active tab)
  initFilesView();

  // Self-service home-directory management (Feat 13)
  $("btn-manage-homedirs").addEventListener("click", () => openHomeDirsModal());

  // Launch modal for file-backed apps (Feat 12 — open file on launch)
  $("launch-cancel").addEventListener("click", () => {
    $("modal-launch").hidden = true;
  });
  $("launch-open-on-launch").addEventListener("change", () => {
    syncLaunchFileFields();
  });
  $("launch-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (launchModalApp) await handleLaunchConfirm(launchModalApp);
  });

  // Pinned launch options (Feat 7)
  $("btn-save-preset").addEventListener("click", () => openSavePresetModal());

  // Upload to shared storage (Feat 10)
  $("btn-upload-storage").addEventListener("click", () =>
    pickFileForStorageUpload($("btn-upload-storage"))
  );

  // Files view "Open in app" launches a session — keep the session list fresh.
  window.addEventListener("vreckan:sessions-changed", () => loadSessions());

  // App Laboratory (Feat 6) creates/updates apps — keep the grid + pinned list fresh.
  window.addEventListener("vreckan:apps-changed", async () => {
    try {
      const appsData = await secureFetch("/api/applications", { method: "POST", body: {} });
      setInstalledApps(appsData);
      renderAppApps(installedApps);
      await loadPinned();
    } catch (e) {
      /* non-fatal — the grid refreshes on the next full load */
    }
  });

  // Admin console view (hidden unless admin; listeners wired regardless)
  initAdminView();

  // Persistent sidebar navigation (Feat 5) — replaces the old topbar tabs.
  const sidebarNav = $("sidebar-nav");
  if (sidebarNav) {
    sidebarNav.addEventListener("click", (e) => {
      const item = e.target.closest("button[data-view], button[data-admin-section]");
      if (!item) return;
      if (item.dataset.view) {
        if (item.id === "tab-files" && item.hidden) return;
        activateView(item.dataset.view);
      } else if (item.dataset.adminSection) {
        if (item.hidden) return;
        activateView("admin", item.dataset.adminSection);
      }
    });
  }
  // The brand header doubles as a "home" link back to the Apps screen.
  $("brand-home")?.addEventListener("click", () => activateView("main"));

  // Try to restore an existing session (cookie)
  try {
    currentUser = await me();
    // SSO users authenticate through the identity provider, so their password
    // is managed externally — disable the self-service change-password control.
    if (currentUser && currentUser.is_sso) {
      const cpBtn = $("btn-change-password");
      if (cpBtn) {
        cpBtn.disabled = true;
        cpBtn.title = "Your password is managed by your SSO provider.";
      }
    }
    // The language selector only makes sense once authenticated (the choice is
    // persisted to the account). Reveal it and honor the DB-stored language,
    // which takes precedence over the browser/localStorage value used above.
    if (langWrap) langWrap.hidden = false;
    const dbLang = currentUser?.settings?.ui_language;
    if (dbLang && resolveLanguage(dbLang) !== activeLanguage()) {
      await setLanguage(dbLang);
      try {
        await refreshMainLists();
        await loadPinned();
        if (currentUser.is_admin) refreshAdminSection();
      } catch (_) {}
    }
    if (langSelect) langSelect.value = activeLanguage();
    await enterMain();
  } catch (e) {
    showView("login");
  }
}

// Persist the chosen UI language to the account (DB) so it follows the user
// across browsers/devices. The dropdown already previews the language locally;
// this makes it stick.
async function persistUiLanguage(lang) {
  const btn = $("lang-apply");
  if (btn) btn.disabled = true;
  try {
    const res = await fetch("/api/auth/preferences", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ ui_language: lang }),
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    toast(t("common.saved"));
  } catch (err) {
    toast(err.message || "Could not save language", "error");
  } finally {
    if (btn) btn.disabled = false;
  }
}

// `init` is exported (not called here) so the entry module (entry.js) can
// invoke it exactly once. Calling it here would run it in EVERY module
// instance of app.js — and the sub-modules (app/*.js) import app.js under a
// different URL than the entry point, which would create a second instance
// and double-bind every listener (e.g. two launch POSTs per click).
export { init };
