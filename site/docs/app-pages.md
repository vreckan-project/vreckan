---
layout: default
title: App Pages
nav: app-pages
description: A page-by-page walkthrough of the Vreckan web app — what each page does, how you interact with it, and what every setting means.
---

<div class="doc">
  <h1>App Pages</h1>
  <p class="doc-sub">A complete walkthrough of every page in the Vreckan web app, in sidebar order: the user views first (Apps, Files), then the admin sections in their three sidebar groups (Access, Apps, System).</p>

  <h2>How the app is organized</h2>
  <p>The web client is a single-page app with a persistent left sidebar. After you sign in (username and password, or single sign-on when the administrator has enabled it), you land on the <strong>Apps</strong> page. The sidebar is organized in groups, and entries appear or disappear based on your settings and permissions:</p>
  <table>
    <thead>
      <tr><th>Sidebar group</th><th>Pages</th><th>Who sees them</th></tr>
    </thead>
    <tbody>
      <tr><td>(top of sidebar)</td><td><strong>Apps</strong>, <strong>Files</strong></td><td>Every signed-in user. Files is shown only when your account has persistent storage enabled.</td></tr>
      <tr><td>Access</td><td>Overview, Accounts, Groups, Roles, SSO</td><td>Anyone with at least one admin permission; each page additionally requires its own <code>admin.&lt;section&gt;</code> permission.</td></tr>
      <tr><td>Apps</td><td>Control, Templates <em>(BETA)</em>, Laboratory <em>(BETA)</em></td><td>Same rule: per-page admin permission.</td></tr>
      <tr><td>System</td><td>Global Settings, Volume mounts, Backup, Sessions, Certificates</td><td>Same rule: per-page admin permission.</td></tr>
    </tbody>
  </table>
  <p>Permissions come from three sources that are unioned — direct permissions, roles, and groups (the most permissive source wins); see the <a href="#access">Access pages</a>. The <code>admin</code> super-permission unlocks every admin section at once.</p>
  <p>The sidebar remembers where you were: refreshing the browser lands you back on the page you were on, and clicking the Vreckan brand at the top of the sidebar always takes you back to Apps. At the bottom of the sidebar you see your username, a <strong>Password</strong> button (change your own password; shown automatically at first sign-in while you still carry the bootstrap password; disabled for SSO accounts, whose password is managed by the identity provider), a <strong>language</strong> selector with an <strong>Apply</strong> button (the choice is persisted to your account and follows you across browsers and devices), and <strong>Sign out</strong>.</p>

  <h2 id="apps">Apps</h2>
  <p>The landing page and the heart of the app: a grid of every installed application you're allowed to use, plus your active sessions and saved launch presets.</p>

  <h3>App grid</h3>
  <p>Each tile shows the app's icon, name, and capability badges:</p>
  <ul>
    <li><strong>Laboratory</strong> — the app is a custom app created in the Laboratory (a "meta" app built on top of another app).</li>
    <li><strong>GPU</strong> — the app can use a GPU (NVIDIA or DRI3); a GPU choice appears in the launch dialog when the server has GPUs.</li>
    <li><strong>URL</strong> — the app can open a URL (browsers).</li>
    <li><strong>Files</strong> — the app is file-backed: it can open files from your home directories.</li>
  </ul>
  <p>Clicking a tile opens the <strong>launch dialog</strong> for that app.</p>

  <h3>Launch dialog</h3>
  <p>Every app uses the same dialog; the fields shown depend on the app's capabilities. Standing defaults are pre-filled from your <em>default preset</em> (see Pinned), falling back to the administrator's global default GPU for GPU-capable apps.</p>
  <table>
    <thead>
      <tr><th>Field</th><th>What it does</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Session name</strong> (optional)</td><td>Appears in the session's window title and in the active-sessions list, so you can tell sessions apart.</td></tr>
      <tr><td><strong>URL to open</strong></td><td>Shown for URL-capable apps. Required for apps with no file support; optional when the app is also file-backed.</td></tr>
      <tr><td><strong>Open file on launch</strong></td><td>Shown for file-backed apps. Upload a file from your computer, or name a file already in shared storage. The file is copied into the session's storage and opened in the app. An <em>Upload a file to shared storage</em> button (shown when you have persistent storage) puts a file into shared storage first.</td></tr>
      <tr><td><strong>Home directory</strong></td><td><em>Auto</em> — a per-app persistent home (<code>auto-&lt;app-name&gt;</code>), created on demand. <em>Cleanroom</em> — ephemeral storage that is wiped when the session ends. Or one of your named home directories. A <em>Manage</em> button (persistent storage only) opens the home-directory list, where you can create, browse, and delete your own home directories.</td></tr>
      <tr><td><strong>Language</strong></td><td>The locale the app runs under (e.g. <code>de_DE.UTF-8</code>); pre-filled from your browser language.</td></tr>
      <tr><td><strong>GPU</strong></td><td>Shown only for GPU-capable apps on servers that report GPUs. <em>Auto</em> (no specific GPU) or a specific device.</td></tr>
      <tr><td><strong>Launch in room mode</strong></td><td>Start the session as a shared room so others can join it.</td></tr>
      <tr><td><strong>Wayland</strong></td><td>On by default. Unchecking it runs the session on X11 instead (the server sets <code>PIXELFLUX_WAYLAND=false</code>, overriding anything the image bakes in).</td></tr>
      <tr><td><strong>Save as preset</strong></td><td>Saves the current options as a reusable preset tied to this app (see Pinned below).</td></tr>
    </tbody>
  </table>
  <p>Clicking <strong>Launch</strong> starts an isolated container on the server and opens the app in a new tab through the secure session proxy. Each session is reached through a unique, single-use URL — the app container is never directly exposed to the internet.</p>

  <h3>Pinned (launch presets)</h3>
  <p>The <strong>Pinned</strong> section at the top of the page is hidden until you save a preset. Each entry shows the preset's name, which app it's tied to (or <em>All apps</em> when it's your default), and its trigger (manual, all URLs, or a file extension). Per preset you can:</p>
  <ul>
    <li><strong>Apply</strong> — fill the launch dialog's options with the saved values.</li>
    <li><strong>Make default</strong> — your default preset is restored on every login, and its options (GPU, home directory, language, …) are used across <em>all</em> apps, not just the one it was saved for. Only one preset can be your default.</li>
    <li><strong>Delete</strong> — remove the preset; the standing values fall back to the remaining default or the built-in defaults.</li>
  </ul>

  <h3>Active sessions</h3>
  <p>The list of sessions you have running, newest activity included. Each row shows the app, the session name, when it started, a <em>Room mode</em> marker for shared rooms, an <em>Out of date</em> marker when a newer image is available for the app, and a badge with the count of active share links (clicking it opens the share-link manager for that session).</p>
  <p>Actions on each session:</p>
  <ul>
    <li><strong>Open</strong> — reopen the session in a new tab.</li>
    <li><strong>Share</strong> — create public links to the session without giving the recipient a Vreckan account: <em>Full access</em> (they can use the app) or <em>View-only</em> (they watch, no input), with file permissions (browse / download / upload), presets, an optional password, and an expiry (1 h, 24 h, 7 d, or custom). Multiple links can coexist; each can be copied, edited, or revoked.</li>
    <li><strong>Recreate</strong> — appears when the session is out of date; rebuilds it from the latest image (the app briefly goes offline).</li>
    <li><strong>Send File</strong> — upload a file from your computer directly into the running session.</li>
    <li><strong>Kill</strong> — end the session and free its resources.</li>
  </ul>

  <h2 id="files">Files</h2>
  <p>A full file manager over your persistent home directories. The page is visible only when your account has persistent storage enabled.</p>
  <ul>
    <li><strong>Home directory selector</strong> (top left) — switch between your home directories. Internal directories (<code>auto-&lt;app&gt;</code> homes and the shared-files store) are hidden from the selector.</li>
    <li><strong>Breadcrumb / Up</strong> — click a folder to enter it, click a crumb or <em>Up</em> to go back. <em>Refresh</em> re-lists the current directory.</li>
    <li><strong>Table</strong> — one row per entry with a selection checkbox, name, size, and modified time; 50 entries per page with Prev/Next pagination. A select-all checkbox covers the current page.</li>
  </ul>
  <p>With one or more entries selected, the toolbar actions are enabled:</p>
  <table>
    <thead>
      <tr><th>Action</th><th>What it does</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>New folder</strong></td><td>Create a folder in the current directory.</td></tr>
      <tr><td><strong>Upload</strong></td><td>Upload one or more files from your computer into the current directory (chunked, with per-file progress).</td></tr>
      <tr><td><strong>Download</strong></td><td>Download the single selected file (streamed over the encrypted channel).</td></tr>
      <tr><td><strong>Share</strong></td><td>Create a public link to the single selected file. You can set an optional password and an optional expiry in hours; the resulting link can be copied from the dialog and later managed from the Shares panel.</td></tr>
      <tr><td><strong>Open in app</strong></td><td>Launch a file-backed installed app with the selected file: pick the app, confirm <em>Open file on launch</em>, and the session opens in a new tab with the file copied into its storage.</td></tr>
      <tr><td><strong>Delete</strong></td><td>Delete the selected entries (irreversible; runs as a background task with status reporting).</td></tr>
      <tr><td><strong>Shares</strong></td><td>Open the <em>Active shares</em> panel: every public file link you created — file, size, created, expiry, password indicator, the link itself — with Copy and Revoke per link.</td></tr>
    </tbody>
  </table>

  <h2 id="access">Access (admin group)</h2>

  <h3>Overview</h3>
  <p>The landing page of the admin area — visible to anyone who can see the admin area at all. It has two cards:</p>
  <ul>
    <li><strong>Server status</strong> — who you are (username, whether you're an admin), the server's CPU model, total/used disk, the GPUs it sees, and the API port. A <strong>How to Use Vreckan</strong> button opens a short built-in guide: launching an app, opening a file in an app, managing persistent storage, saving launch options, and the admin toolkit.</li>
    <li><strong>My Settings</strong> — edit <em>your own</em> account settings (self-service; administrators edit other people's settings from Accounts):
      <ul>
        <li><strong>Group</strong> — your primary group.</li>
        <li><strong>Storage limit (MB)</strong> — cap on your persistent storage; <code>-1</code> = unlimited.</li>
        <li><strong>Session limit</strong> — cap on your simultaneously running sessions; <code>-1</code> = unlimited.</li>
        <li><strong>Settings toggles</strong> (collapsible): <em>Active</em> (the account can sign in), <em>Persistent storage</em> (home directories and the Files page), <em>Public sharing</em> (file share links), <em>Harden container</em>, <em>Harden Openbox</em>, <em>GPU</em> (the account may use a GPU).</li>
      </ul>
    </li>
  </ul>

  <h3>Accounts</h3>
  <p>The unified roster of every account — administrators first, then regular users — with one card per account and an <strong>Add Account</strong> form above the list.</p>
  <p>Each card shows the username with badges summarizing the account: <em>admin</em> (administrators), <em>SSO</em> (authenticates through the identity provider), active/inactive, password set or none (SSO accounts have no local password), persistent storage on/off, public sharing on/off, GPU on/off, primary group, assigned roles, storage and session limits, the account's SSO groups (highlighted when a group grants admin), and its home directories.</p>
  <p>Actions on each card:</p>
  <ul>
    <li><strong>Edit</strong> — opens an edit form in place of the card: the Admin checkbox (locked on for the bootstrap <code>admin</code> account, which cannot be demoted), the group selector, storage/session limits, and the settings toggles.</li>
    <li><strong>Access</strong> — the account's effective access, edited in one modal from three sources: <em>Roles</em>, <em>Direct permissions</em> (the admin.* and user.* permission catalog), and <em>Groups</em>. The effective access is the union of all three — the most permissive wins.</li>
    <li><strong>Set password</strong> — set or reset the account's web-login password (minimum 8 characters); it takes effect immediately. Hidden for SSO accounts.</li>
    <li><strong>Home dirs</strong> — list, add, and remove the account's home directories.</li>
    <li><strong>Delete</strong> — remove the account.</li>
  </ul>
  <p>The Add Account form takes a username, an optional <em>Admin</em> checkbox, and the same settings form, so a new account can be fully configured at creation.</p>

  <h3>Groups</h3>
  <p>Groups bundle settings, roles, and permissions and apply them to every member. One card per group, with an <strong>Add Group</strong> form above.</p>
  <ul>
    <li>Each card shows the group's status badges (active, storage, GPU), the <strong>roles granted to every member</strong>, and the <strong>direct permissions granted to every member</strong>, plus its storage limit. Groups that were observed from SSO are badged; groups in the admin SSO list are highlighted.</li>
    <li><strong>Add / Edit</strong> a group: name (letters, numbers, <code>-</code>, <code>_</code>), the settings toggles and limits, the roles to grant every member, and the permissions to grant every member.</li>
  </ul>
  <p>Groups observed from the identity provider appear here automatically on SSO login; the roles/permissions you grant them here decide what those users get. Which provider groups count as admin or user groups is configured on the <a href="#sso">SSO page</a>.</p>

  <h3>Roles</h3>
  <p>Roles are named bundles of permissions that can be assigned to accounts (via Accounts → Access) and to groups. The table lists every role with its description and permissions.</p>
  <ul>
    <li><strong>Create role</strong> — name, description, and permission checkboxes drawn from the permission catalog (two collapsible sections: admin permissions and user permissions, each with select-all).</li>
    <li><strong>Edit</strong> — change a role's description and permissions.</li>
    <li><strong>Delete</strong> — available for custom roles; built-in roles (e.g. <code>admin</code>, <code>operator</code>, <code>user</code>) are editable but cannot be deleted. Deletion is refused while the role is still assigned to a user or a group.</li>
  </ul>

  <h3 id="sso">SSO / OIDC</h3>
  <p>Connect Vreckan to an OpenID Connect provider (authentik, Keycloak, Entra ID, …). Discovery is fetched from <code>{issuer}/.well-known/openid-configuration</code>. Every field carries a badge showing where its current value comes from — <em>db</em> (saved here), <em>env</em> (the <code>VRECKAN_OIDC_*</code> environment), or <em>default</em>; a value saved on this page overrides the environment.</p>
  <table>
    <thead>
      <tr><th>Setting</th><th>What it does</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Enable SSO login</strong></td><td>Shows the "Sign in with SSO" button on the login screen.</td></tr>
      <tr><td><strong>Issuer URL</strong></td><td>Base URL of the identity provider.</td></tr>
      <tr><td><strong>Client ID / Client secret</strong></td><td>The OIDC client registration. The secret is a password field; leaving it blank keeps the current value (optional for public clients).</td></tr>
      <tr><td><strong>Redirect URI</strong></td><td>Where the provider sends the user back; defaults to <code>{base}/api/auth/oidc/callback</code>.</td></tr>
      <tr><td><strong>Scopes</strong></td><td>OIDC scopes requested at authorization (e.g. <code>openid profile email groups</code>).</td></tr>
      <tr><td><strong>Auto-provision new users</strong></td><td>Create a Vreckan account automatically the first time an SSO user signs in.</td></tr>
      <tr><td><strong>State TTL (seconds)</strong></td><td>How long the single-use OAuth state is held (default 600).</td></tr>
      <tr><td><strong>Test connection</strong></td><td>Contacts the provider and reports whether it is reachable, plus the authorization/userinfo/token endpoints it advertised.</td></tr>
    </tbody>
  </table>
  <p><strong>Group mapping.</strong> On each SSO login a user is joined to a Vreckan group for every provider group they belong to. Two tag lists control what those groups mean:</p>
  <ul>
    <li><strong>Admin groups</strong> — members are auto-promoted to Vreckan admin (grants the <code>admin</code> role).</li>
    <li><strong>User groups</strong> — members get the built-in <code>user</code> role; the first of these a user belongs to becomes their primary group.</li>
  </ul>
  <p>Any other (newly observed) provider group starts with no permissions until you grant roles or permissions to the matching Vreckan group on the <a href="#groups">Groups page</a>.</p>

  <h2 id="apps-admin">Apps (admin group)</h2>

  <h3>Control (installed apps)</h3>
  <p>The management page for installed applications: what is installed, which image each app runs, who may use it, and how to keep it current. It also hosts the app-store browser and store configuration.</p>
  <p><strong>Installed apps table.</strong> One row per installed app: name, source (store / app id), whether it's file-backed, the allowed users and groups, and the image cell — the image reference, a short SHA of the running image, and a status badge (<em>Up to date</em>, <em>Update available</em>, <em>Pulling</em>/<em>Waiting to start</em> with a live progress bar, or <em>Pull failed</em>).</p>
  <p>Per-app actions:</p>
  <table>
    <thead>
      <tr><th>Action</th><th>What it does</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Edit</strong></td><td>Set the <em>URL environment variable</em> (the env var the app reads the launch URL from — e.g. <code>CHROME_CLI</code> for Chrome; leave empty if the app doesn't open URLs) and <em>who can use this app</em>: an "All users" master switch, or pick specific users and groups. An app nobody is selected for is hidden from and unusable by everyone.</td></tr>
      <tr><td><strong>Check</strong></td><td>Query the registry for a newer image of this app (no pull is started); the row's status updates immediately.</td></tr>
      <tr><td><strong>Update</strong></td><td>Pull the latest image; a progress bar tracks the download in the row until it completes or fails.</td></tr>
      <tr><td><strong>Auto</strong> checkbox</td><td>Opt this app in to the auto-update job (below).</td></tr>
      <tr><td><strong>Delete</strong></td><td>Remove the app from the platform.</td></tr>
    </tbody>
  </table>
  <p><strong>Bulk actions.</strong> <em>Check all</em> queries the registry for every installed app and reports how many have an update available. <em>Update all</em> starts a background pull of the latest image for every app and follows the aggregate progress in the table.</p>
  <p><strong>Auto-update job.</strong> An enable toggle plus a check interval (1 / 6 / 12 / 24 hours). When enabled, the server periodically checks the opted-in apps and pulls newer images automatically. The status line reports the next check time, the last check time, and how many of the installed apps are opted in.</p>
  <p><strong>Install App From Store.</strong> Pick a configured store, optionally type a search term (filters the loaded list live), and <em>Load</em> to fetch the store's catalog. Each listed app has an <strong>Install</strong> button that opens a modal: choose which <em>app template</em> the app will use, choose <em>who can use this app</em> (All users, or specific users/groups), then Install downloads the container image with a live progress bar — the modal can be closed while the download continues in the background.</p>
  <p><strong>App Stores</strong> (the cards below the installer). Add a store with a name and the URL of its YAML catalog file (e.g. <code>https://…/store.yml</code>); configured stores are listed with a delete action. The store browser above only knows about stores configured here.</p>

  <h3>Templates <span class="nav-beta">BETA</span></h3>
  <p>App templates define the environment variables passed to session containers. Only values that differ from the base layer are stored — everything else follows the base layer at launch. Templates are assigned to apps (e.g. at install time) and shape how those apps behave for everyone.</p>
  <ul>
    <li><strong>Template selector / name</strong> — pick an existing template, or enter a name for a new one. <em>Save</em> stores the template; <em>Delete</em> removes it (the <code>Default</code> template cannot be deleted — it is recreated on every server start).</li>
    <li><strong>The form</strong> is organized into categories: <em>UI Settings</em> (client title, logo, which sidebar sections and buttons the client shows), <em>App Settings</em>, <em>Hardening</em>, <em>General &amp; Performance</em>, <em>WebRTC &amp; TURN</em>, and <em>Docker Advanced Overrides</em>. Every field is pre-filled from the base layer's effective value, so the form mirrors exactly what an app would receive.</li>
    <li><strong>WebRTC note</strong> — sessions stream over WebSockets by default. Setting any STUN, TURN, Cloudflare, or public-IP value, or choosing WebRTC mode, switches the container to WebRTC with dual mode enabled so users can still fall back to WebSockets.</li>
    <li><strong>Docker overrides</strong> change the default container configuration — the page recommends making a backup first if you touch them.</li>
    <li><strong>UI Preview</strong> — a live mock of the Selkies client that redraws as you change UI settings (title, logo, sidebar sections, keyboard button), so you can see the effect before saving.</li>
  </ul>
  <p>The <strong>base layer</strong> — the per-setting defaults that every launch starts from, before per-app templates and per-launch choices override them — is edited on the <a href="#global-settings">Global Settings page</a>, not here.</p>

  <h3>Laboratory <span class="nav-beta">BETA</span></h3>
  <p>Create a custom app from an installed base app — or edit one you already have. The new app inherits the base application's image and provider settings, so it "is" the base app with a different identity and configuration; it appears on the Apps page with a <em>Laboratory</em> badge.</p>
  <p>The form:</p>
  <ul>
    <li><strong>App to customize</strong> — pick an existing custom app to edit, or "Create new".</li>
    <li><strong>Base application</strong> — for new apps: which installed app to build from (selecting one copies its autostart scripts and icon into the form).</li>
    <li><strong>App name</strong> and <strong>App icon</strong> (PNG upload with a live preview).</li>
    <li><strong>Autostart scripts</strong> — X11 and Wayland shell scripts run when the session starts (they can reference <code>$VRECKAN_FILE</code> / <code>$VRECKAN_URL</code>).</li>
    <li><strong>Allowed users / groups</strong> (comma-separated; <code>all</code> by default) and a <strong>Wayland mode</strong> toggle.</li>
    <li><strong>Save changes</strong> — persist edits to the custom app (enabled once the form is dirty).</li>
    <li><strong>Launch &amp; modify</strong> — opens a live session embedded in the page whose home directory <em>is the app's home template</em>: changes you make inside the session (config files, data) are written into the template and therefore apply to all future sessions of that app. The button becomes <em>Close session</em> when the customization session is running.</li>
  </ul>

  <h2 id="system">System (admin group)</h2>

  <h3 id="global-settings">Global Settings</h3>
  <p>Server-wide settings that apply across all accounts and apps. Two cards:</p>
  <ul>
    <li><strong>Global Default GPU</strong> — the fallback GPU for launches where the user doesn't pick one (and has no preset covering it). A dropdown of the server's GPUs, or <em>None (no global default)</em>. Apps that don't support the GPU type are unaffected.</li>
    <li><strong>Advanced — Base layer (template schema)</strong> — the editor for the base-layer defaults that every launch starts from. Each setting row has: the variable <em>name</em> (fixed for existing settings; remove + add to rename), <em>label</em>, <em>description</em>, <em>category</em>, <em>type</em> (text / boolean / select, with per-line <code>value:label</code> options for selects), a <em>docker</em> flag (such settings become container run-options instead of environment variables), the <em>default</em> (the image baseline — reference-only for built-in settings), and the <em>current</em> base-layer override (empty = "use default", which lets the image's built-in default apply). <em>Add setting</em> appends a new row; <em>Save base layer</em> persists everything. Changes apply to new launches. The page carries an ADVANCED warning recommending a backup before changing anything (with a shortcut to the Backup page).</li>
  </ul>

  <h3>Volume mounts</h3>
  <p>Bind directories from the Docker host into app containers. The container path is <em>relative to the user's home directory inside the container</em>, so the same mount composes with each user's storage. The table lists every mount: name, host path, container path, mode (<em>ro</em>/<em>rw</em>), and assignment (none = inactive, or <em>user: …</em> / <em>group: …</em> — only the assigned users or groups get the mount in their sessions).</p>
  <p><strong>Add / Edit</strong> a mount: name, host path, container path, a <em>Read-only</em> checkbox, and <em>Assign to</em> (None, a specific User, or a specific Group, with a target picker that follows the scope). A mount assigned to no one is stored but never attached.</p>

  <h3>Backup</h3>
  <p>Snapshot and restore of the application configuration. Three cards:</p>
  <ul>
    <li><strong>Create Backup</strong> — creates a <code>tar.gz</code> of the configuration in <code>/data/backups</code>; check <em>Include user data (home directories)</em> to snapshot users' files too. Restoring re-applies the backup to the running server without a restart.</li>
    <li><strong>Backups</strong> — the list of stored backups (name, size, created, whether user data is included) with three actions each: <em>Download</em> (the archive streams over the encrypted channel to your computer), <em>Restore</em> (overwrites the configuration files contained in the archive and reloads them; active sessions are not affected — but if the archive is older than the current admin key, you could lock yourself out), and <em>Delete</em>.</li>
    <li><strong>Restore From File</strong> — upload a previously downloaded backup <code>tar.gz</code> to restore it, with the same overwrite-and-reload semantics; files not present in the archive are left unchanged.</li>
  </ul>

  <h3>Sessions</h3>
  <p>Every live session on the platform, grouped by the user who owns it. Each row shows the session (app name, session name, session id), when it started, whether it's a room-mode session, and an <em>Out of date</em> badge when a newer image is available for its app.</p>
  <p>Actions per session: <strong>Connect</strong> (open the session in a new tab — an administrator can view any user's session), <strong>Recreate</strong> (rebuild the session from the latest image; the app briefly goes offline), and <strong>Kill</strong> (end the session).</p>

  <h3>Certificates</h3>
  <p>Issue and renew ACME certificates with the built-in acmex client, and see the state of the certificate currently serving the platform. When ACME is disabled (or no certificate exists), the server falls back to a self-signed certificate.</p>
  <p><strong>Current certificate</strong> (top card): the source of the active certificate (database or environment), its expiry, the days remaining (or how long ago it expired), and a Valid / Expired status badge.</p>
  <p><strong>ACME configuration</strong> — every field carries a source badge (<em>db</em> / <em>env</em> / <em>default</em>); a value saved here overrides the environment.</p>
  <table>
    <thead>
      <tr><th>Setting</th><th>What it does</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Enable ACME</strong></td><td>Turn on ACME issuance; off = self-signed certificate.</td></tr>
      <tr><td><strong>Domains</strong></td><td>The domain(s) to issue certificates for (tag input; stored comma-separated).</td></tr>
      <tr><td><strong>Certificate authority</strong></td><td>Which ACME CA issues the certificate: Let's Encrypt, ZeroSSL, or Google. An <em>Allow plaintext http:// CA</em> checkbox is for custom CA directory URLs only (by default <code>https://</code> is required so the account-key JWSes and CA responses stay encrypted in transit).</td></tr>
      <tr><td><strong>Use the staging server</strong></td><td>Offered for Let's Encrypt only: test certificates from the staging CA, with no production rate limits.</td></tr>
      <tr><td><strong>Auth method</strong></td><td><em>DNS-01</em> works behind a reverse proxy; <em>HTTP-01</em> requires the ACME server to reach your domain over HTTP.</td></tr>
      <tr><td><strong>DNS-01 provider</strong></td><td>The DNS provider that fulfils the DNS-01 challenge (e.g. Cloudflare).</td></tr>
      <tr><td><strong>Cloudflare fields</strong></td><td>Shown for the Cloudflare provider: zone ID (leave blank to look it up from the API token), the ACME account email (optional with a token), and the API token itself (needs Zone.DNS edit permission; leave blank to keep the current token).</td></tr>
      <tr><td><strong>Provider config (JSON)</strong></td><td>For non-Cloudflare DNS providers: the provider's credentials as JSON (e.g. <code>api_token</code>, <code>hosted_zone_id</code>), per the provider's documentation. Leave blank to keep the current config.</td></tr>
      <tr><td><strong>Auto-renew</strong></td><td>Renew certificates automatically in the background.</td></tr>
      <tr><td><strong>Check every (hours)</strong></td><td>How often the background job checks whether the certificate needs renewing (default 6).</td></tr>
      <tr><td><strong>Renew within (days)</strong></td><td>Renew when the certificate expires within this many days (default 30).</td></tr>
      <tr><td><strong>Issue / Renew now</strong></td><td>Trigger issuance or renewal immediately, with the outcome and any error/log detail shown inline.</td></tr>
    </tbody>
  </table>
</div>
