---
layout: default
title: Usage
nav: usage
description: Launching apps, managing files and shares, and the administrator's toolkit in Vreckan.
---

<div class="doc">
  <h1>Usage</h1>
  <p class="doc-sub">Day-to-day with Vreckan — as a user and as an administrator.</p>

  <h2>Launching an application</h2>
  <ol>
    <li>Sign in to the web client.</li>
    <li>Open the <strong>Apps</strong> view and pick an application from the installed list (or install one from a store). A <strong>Launch</strong> dialog opens for that app.</li>
    <li>Set the options you want — the fields shown depend on the app type — optionally save them as a preset, and click <strong>Launch</strong>.</li>
    <li>Vreckan spins up an isolated container and opens the app in a new tab through the secure session proxy. When you're done, end the session to free the resources.</li>
  </ol>
  <div class="callout">
    <strong>Single-use URLs.</strong> Each session is reached through a unique, single-use URL that the server hands to your browser. The internal app container is never directly exposed to the internet.
  </div>

  <h3>What the Launch dialog shows</h3>
  <p>Every app opens the same per-app Launch dialog; only the extra fields differ, based on what the app can do:</p>
  <table>
    <thead>
      <tr><th>Field</th><th>Browser apps</th><th>File apps</th><th>Plain apps</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Session name</strong> (optional)</td><td>Shown</td><td>Shown</td><td>Shown</td></tr>
      <tr><td><strong>URL to open</strong></td><td>Shown (required)</td><td>—</td><td>—</td></tr>
      <tr><td><strong>Open file on launch</strong> — upload a file from your computer, or name a file already in shared storage</td><td>—</td><td>Shown (optional)</td><td>—</td></tr>
      <tr><td><strong>Launch options</strong> — home directory, language, GPU, room mode, Wayland</td><td>Shown</td><td>Shown</td><td>Shown</td></tr>
    </tbody>
  </table>
  <p>The <strong>Session name</strong> (optional) appears in the session's window title and in the active-sessions list, so you can tell sessions apart — most useful for browser sessions, where one app can be pointed at many different sites.</p>
  <p>For file apps, the file section also has an <em>Upload a file to shared storage</em> button (hidden when persistent storage is off) if you first need to get a file into shared storage to open it.</p>

  <h3>Launch options</h3>
  <p>The options that used to sit in the "Launch options" card at the top of the Apps view are now inside the Launch dialog, so each app is configured right where you launch it. The card in the top bar is gone:</p>
  <ul>
    <li><strong>Home directory</strong> — <em>Auto</em> (a per-app persistent home), <em>Cleanroom</em> (ephemeral storage, wiped after the session), or a named home directory; a <em>Manage</em> link opens the home-directory list (shown when persistent storage is enabled).</li>
    <li><strong>Language</strong> — the locale the app runs under (for example <code>de_DE.UTF-8</code>); the list is pre-filled from your browser language.</li>
    <li><strong>GPU</strong> — shown only for apps that can use a GPU when the server reports GPUs; pick Auto or a specific device. A device you pick keeps applying to that app on later launches — pick <em>Auto</em> to clear it.</li>
    <li><strong>Room mode</strong> — start the session as a shared room so others can join; the option is honored on every launch path in the dialog, including launching a file-capable app with a file picked in its file section.</li>
    <li><strong>Wayland</strong> — enabled by default. Unchecking it takes the session off Wayland; the server sets the container env accordingly (<code>PIXELFLUX_WAYLAND=false</code>, overriding any value the image bakes in).</li>
  </ul>

  <h3>Presets</h3>
  <p>The dialog's <strong>Save as preset</strong> button saves the current options for that app. Saved presets appear in the <strong>Pinned</strong> section at the top of the Apps view (it is hidden until you have one), where you can apply, make default, or delete them. Each time the Launch dialog opens it pre-fills from your default preset (falling back to the admin's global default GPU when you have none), so your usual settings are one click away.</p>

  <h2>Files &amp; sharing</h2>
  <ul>
    <li>The <strong>Files</strong> view is a full file manager over your persistent home directory: create folders, upload, download, rename, and delete.</li>
    <li>Open any file directly in a remote application with <em>Open in app</em>.</li>
    <li>Share a file or folder with a <strong>public link</strong> — revoke it at any time. Expired shares are cleaned up automatically.</li>
    <li>Multiple <strong>home directories</strong> can be configured, so different apps can see different slices of your storage.</li>
  </ul>

  <h2>For administrators</h2>
  <p>The admin area is a complete management dashboard, reachable from the sidebar. It is gated by fine-grained permissions, so you can grant exactly the control each person needs.</p>
  <div class="grid">
    <div class="card">
      <h3><span class="glyph">👥</span> People &amp; Groups</h3>
      <p>Create and manage users, set passwords, assign groups, and map SSO groups to Vreckan roles.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">🔑</span> Roles &amp; Permissions</h3>
      <p>Define roles and assign the permission catalog — from <code>admin.apps</code> down to <code>user.gpu</code>.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">📦</span> Apps &amp; Stores</h3>
      <p>Install apps from stores, edit app definitions, and manage the app stores themselves.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">🧩</span> Templates</h3>
      <p>Shape the per-app experience with templates and a configurable template schema.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">🔌</span> Volume Mounts</h3>
      <p>Attach external volumes into app containers for shared or specialized storage.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">💾</span> Backup &amp; Restore</h3>
      <p>Snapshot and restore the entire configuration database on a schedule or on demand.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">🔐</span> SSO / OIDC</h3>
      <p>Connect an identity provider (authentik, Keycloak, Entra ID, …) and map its groups to admins and users.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">📜</span> Certificates</h3>
      <p>Issue and renew ACME certificates, and view the current certificate's status and expiry.</p>
    </div>
  </div>

  <h2>Sessions &amp; the laboratory</h2>
  <p>Admins can see every live session across the platform and stop any of them. The <strong>Laboratory</strong> gives a live console into a running session for hands-on debugging.</p>
</div>
