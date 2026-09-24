// Navigation (Feat 5 — persistent sidebar): view switching, sidebar state,
// and main-list refresh.
import { $ } from "../util.js";
import { secureFetch } from "../api.js";
import { activateAdminSection } from "../admin.js";
import { loadHomedirs } from "../files.js";
import { installedApps, renderAppApps, setInstalledApps } from "../app.js";
import { renderSessions } from "./sessions.js";
import { loadPinned } from "./pinned.js";

// Feat 5: the persistent sidebar highlights the active destination. For the
// admin view we track the selected admin section here so the correct sidebar
// item stays highlighted (admin.js owns the actual section state).
export let currentAdminSection = "overview";

function setSidebarActive(view) {
  const nav = $("sidebar-nav");
  if (!nav) return;
  nav.querySelectorAll(".nav-link").forEach((b) => b.classList.remove("active"));
  if (view === "main") $("tab-apps")?.classList.add("active");
  else if (view === "files") $("tab-files")?.classList.add("active");
  else if (view === "admin") {
    const btn = nav.querySelector(`.nav-link[data-admin-section="${currentAdminSection}"]`);
    if (btn) btn.classList.add("active");
  }
}

function showView(name) {
  $("view-login").hidden = name !== "login";
  const shell = $("shell");
  if (shell) shell.hidden = name === "login";
  $("view-main").hidden = name !== "main";
  $("view-files").hidden = name !== "files";
  $("view-admin").hidden = name !== "admin";
  setSidebarActive(name);
}

// Switch the visible view. For the admin view, also (re)render the requested
// admin section so a sidebar click lands on the right panel. The main and
// files views re-fetch their data on activation, so switching pages never
// shows a stale list (e.g. a session that started while you were on another
// page). The launch-options form is intentionally left alone so in-progress
// selections survive a page switch.
function activateView(view, adminSection) {
  if (adminSection) currentAdminSection = adminSection;
  // Remember where the user is so a refresh can restore this page. The admin
  // section is tracked separately because the sidebar only stores the top-level
  // view, not which admin panel is open.
  try {
    localStorage.setItem(
      "vreckan.lastView",
      JSON.stringify({ view, adminSection: view === "admin" ? currentAdminSection : undefined })
    );
  } catch (e) {
    /* storage unavailable — refresh will just land on Apps */
  }
  showView(view);
  if (view === "admin") activateAdminSection(currentAdminSection);
  else if (view === "main") refreshMainLists();
  else if (view === "files") loadHomedirs(true);
}

// Re-fetch the main view's data lists (app grid, active sessions, pinned
// launch options) without touching the launch-options form.
async function refreshMainLists() {
  try {
    const [appsData, sessionsData] = await Promise.all([
      secureFetch("/api/applications", { method: "POST", body: {} }),
      secureFetch("/api/sessions", { method: "GET" }),
    ]);
    setInstalledApps(appsData);
    renderAppApps(installedApps);
    renderSessions(Array.isArray(sessionsData) ? sessionsData : []);
    await loadPinned();
  } catch (e) {
    /* non-fatal — the lists refresh on the next full load */
  }
}

export { setInstalledApps, setSidebarActive, showView, activateView, refreshMainLists };
