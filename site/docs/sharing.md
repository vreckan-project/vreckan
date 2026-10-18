---
layout: default
title: Sharing a session
nav: sharing
description: Share a running app session with anyone — zero-setup quick links, managed share links with per-link capabilities, room invite links, and the trust model to know what you're extending.
---

<div class="doc">
  <h1>Sharing a session</h1>
  <p class="doc-sub">Share a running app session with anyone — no Vreckan account needed. Three ways to do it, one trust model to understand.</p>

  <table>
    <thead>
      <tr><th>Method</th><th>What the recipient gets</th><th>Control you keep</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Quick links</strong> (the session URL, <code>#shared</code>, <code>#player2</code>–<code>#player4</code>)</td><td>Full control, view-only, or gamepad co-play — zero setup</td><td>Only by stopping the session</td></tr>
      <tr><td><strong>Managed share links</strong> (<code>/share/…</code>)</td><td>Exactly the capabilities you grant: full or view-only, file access, password, expiry</td><td>Per-link edit and revoke, without stopping the session</td></tr>
      <tr><td><strong>Room invite links</strong></td><td>A room session: participant (input handover, gamepad slots) or read-only viewer</td><td>Per-invite-link participant vs read-only; viewers leave when the room ends</td></tr>
    </tbody>
  </table>

  <h2>Quick links — zero setup</h2>

  <h3>Full access — the session URL</h3>
  <p>Every running plain (non-room) session has a URL — the one that opened in the new tab when you launched the app, also reachable from the <strong>Open</strong> action in the Sessions view. That URL is a durable bearer link: anyone who has it gets the <strong>live, fully interactive</strong> session, in any browser, with no Vreckan account. Send it over any channel and the recipient is in.</p>
  <div class="callout">
    <strong>Know what you're sending.</strong> A full share is a shared desktop — two people drive one mouse and keyboard. The link never expires until you stop the session, and stopping the session is the only way to kill the link. The token rides in the URL until the first redirect strips it into a cookie, so it can remain in the recipient's browser history.
  </div>

  <h3>View-only — the <code>#shared</code> link</h3>
  <p>Append <code>#shared</code> to the session URL and the recipient <strong>watches the live session with no keyboard, mouse, or gamepad input</strong>. The input authority is enforced <em>server-side inside the app container</em> — the viewer's input messages are refused whatever the client sends, so a modified client can't cheat. No account, no setup.</p>
  <p>Two caveats:</p>
  <ul>
    <li><code>#shared</code> is honored only by plain (non-room) sessions. Room sessions run in Selkies "secure mode", where the provisioned token's role wins and the URL fragment is ignored.</li>
    <li>A plain session's in-container file browser is open to viewers — a determined viewer can list or download session files. If that matters, use a managed share link with the file toggles set to <em>No file access</em>.</li>
  </ul>

  <h3>Co-play — <code>#player2</code> through <code>#player4</code></h3>
  <p>For co-op browser games, append <code>#player2</code>, <code>#player3</code>, or <code>#player4</code> to the session URL. Each extra player gets their own view and a dedicated gamepad slot in the container. Works today on plain sessions, same caveats as <code>#shared</code>.</p>

  <h2>Managed share links</h2>
  <p>When you need control over who can do what, and for how long, use a managed share link. It works for <strong>plain and room sessions alike</strong>, and the recipient still needs no Vreckan account.</p>

  <h3>Creating a link</h3>
  <ol>
    <li>In the <strong>Sessions</strong> view, click <strong>Share</strong> on a running session.</li>
    <li>Pick the role: <strong>Full</strong> — "they can use the app" — or <strong>View-only</strong> — "they can watch; no input".</li>
    <li>Pick the file access: <strong>Standard view-only</strong> (browse + download), <strong>No file access</strong>, <strong>Full access</strong>, or <strong>Customize…</strong> to toggle browse, download, and upload individually.</li>
    <li>Optionally open <strong>Options</strong> to add a password and/or an expiry (none, 1 h, 24 h, 7 d, or custom hours).</li>
    <li>Click <strong>Create link</strong> — the link (<code>/share/…</code>) appears with a copy button, and the dialog now lists the session's active links.</li>
  </ol>
  <p>The same dialog is where you manage links: clicking an existing link pre-fills the form for editing (the button becomes <em>Save changes</em>), and each row has a two-step <strong>Revoke</strong>. Revoking stops that link's visitors without touching the session or any other link.</p>

  <h3>What the recipient experiences</h3>
  <ul>
    <li>Opening the link takes them straight to the session (a password page first, if you set one). No login, no extension — desktop or phone browser.</li>
    <li><strong>View-only</strong> recipients see the live app with input refused server-side; file browse/download/upload work exactly as you toggled them, enforced by the server for every request.</li>
    <li><strong>Full</strong> recipients get the interactive app — on a plain session they can drive it alongside you; on a room session they join the room's participant flow (input handover, gamepad slots).</li>
  </ul>
  <div class="callout">
    <strong>Per-visit tokens.</strong> Every time a link is opened, a fresh per-visit token is minted for that visit. Two people opening the same link hold <em>different</em> tokens, and revoking or expiring the link strips every token that link ever minted — one leaked link no longer lights up everything.
  </div>
  <p>Expiry runs on a background cleanup schedule, so an expired link dies on its own. And when the session itself ends, all of its links end with it — visitors see "session ended" instead of a dead page.</p>

  <h2>Room invite links</h2>
  <p>Launch with <strong>room mode</strong> and the session becomes a collaboration room: a shared display plus viewer list and chat. The room page offers two kinds of invite links, copyable from the room UI:</p>
  <ul>
    <li><strong>Participant</strong> — can receive mouse/keyboard handover and gamepad slots, in addition to watching.</li>
    <li><strong>Read-only viewer</strong> — watches the display and joins chat; no input.</li>
  </ul>
  <p>A recipient needs no account: opening an invite link mints them a personal token, adds them to the room's viewer list, and redirects them into the room page — where the live display now works for them too, not just the owner's first browser.</p>

  <h2>The trust model</h2>
  <p>Vreckan runs two planes over one HTTPS endpoint, and they are <em>not</em> protected equally:</p>
  <table>
    <thead>
      <tr><th>Plane</th><th>Covers</th><th>Protection</th></tr>
    </thead>
    <tbody>
      <tr><td><strong>Control plane</strong> (management API)</td><td>Login, launching apps, session management, files, share management, admin</td><td>End-to-end encrypted (E2EE) between the client and the server</td></tr>
      <tr><td><strong>Data plane</strong> (the app stream)</td><td>Everything the app does — the screen, keystrokes, files — as it streams through Vreckan's session proxy into the container</td><td>TLS in transit; <strong>not</strong> E2EE — the server operator is in the middle of the stream</td></tr>
    </tbody>
  </table>
  <p>The v1 decision: <strong>viewers may trust the server operator.</strong> Sharing a session extends the same trust you already have in your operator to the person you share with — the app traffic both of you see is visible to the server, so if the recipient must <em>not</em> trust the server operator, v1's answer is data-plane E2EE (planned later hardening, not yet shipped).</p>
  <div class="callout">
    <strong>In one sentence.</strong> The management API is end-to-end encrypted; the app stream is end-to-end <em>transport</em>-encrypted — the server operator can see what flows through it, and so can anyone you give a link to.
  </div>

  <h2>Security notes</h2>
  <ul>
    <li><strong>The link is the capability.</strong> A share link grants exactly what it says — treat it like a credential and send it over a trusted channel. A password is an extra gate for the recipient, not a substitute for a trusted channel.</li>
    <li><strong>The token lives in the URL only briefly.</strong> When a link is first opened, the server redirects and moves the per-visit token into a cookie; after that redirect the URL is clean. So the URL you send is safe, but a URL that still carries a <code>?token=</code> (e.g. from the recipient's history or a bookmark captured mid-redirect) is itself a working credential — advise recipients the same way.</li>
    <li><strong>Revocation is per-link.</strong> Revoking or expiring a link stops exactly that link's visitors, without stopping the session or affecting other links. Stopping the session ends all of its links at once.</li>
    <li><strong>Full share = shared desktop.</strong> Two people driving one desktop contend for input; rooms make the handover explicit (mouse/keyboard transfer is a deliberate action) and give gamepads dedicated slots.</li>
    <li><strong>Viewer file access is your choice.</strong> Quick-link viewers on plain sessions can browse and download session files; managed links let you turn each file capability off, and the server enforces the toggle on every request.</li>
  </ul>
</div>
