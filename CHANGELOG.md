# Vreckan — Changelog

Vreckan is a hard fork of [SealSkin 0.3.2](https://github.com/linuxserver/docker-sealskin).
The full fork analysis — what was removed, rewritten, and added relative to the
upstream tree — lives in [`HISTORY.md`](./HISTORY.md). This changelog tracks
development *after* the fork, newest first. The Python-era entries below date
from the squashed git history of the original fork; the Rust-port era (2026-10)
is committed normally in `vreckan`. The changelog is the authoritative
record of what shipped in each release — see
[`docs/release.md`](./docs/release.md) for the conventions.

## 2026-10-23 — v0.8.7: base-image bump + refreshed build-base cache, no source changes since v0.8.6 (ANT-501)

Nightly base-bump cut. `main` has no commits past the `v0.8.6` tag, so this is a
build-base-only patch release: the shipped image is `v0.8.6`'s source compiled on
the refreshed build base.

- **The build base advanced to the latest `vreckan-base-build` image.** The
  `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277)
  from `sha256:11861969…` (the 2026-10-22 re-bake) to `sha256:d8afdab8…` (the
  2026-10-23 re-bake). The re-bake fired because the upstream `rust:1.99-slim`
  digest moved (`sha256:d5928c68…` → `sha256:aab42391…`); the amd64 layers that
  base contributes — the Debian trixie layer and the Rust toolchain layer — are
  byte-identical across the two images, so `rustc`/`cargo` stay at 1.99 and the
  digest moved at the manifest-list level. What actually changed is our own
  layer set: the `-sys` build-dependency layer re-resolved against the current
  Debian trixie snapshot, and the crates.io/git dependency cache re-baked against
  `main` at `13a3543`, so the warm cache now matches this release's `Cargo.lock`.
  No operator-visible behavior changes — `git log v0.8.6..HEAD` is empty.

## 2026-10-22 — v0.8.6: base-image bump + "SealSkin Apps" default store rename + `VRECKAN_SESSION_PORT` removal (ANT-492; ANT-480)

This release completes the v0.8.6 cut: the nightly base-image update that
landed on `main` (PR #82) ships together with the Rust-side ANT-480 work —
the app-store rename and the session-port variable that was metadata-only in
the Rust port.

- **The build base advanced to the latest `vreckan-base-build` image.** The `Dockerfile` build-stage pin and `.base-image-digest` were re-pointed by the nightly scan bot (PR #82) from `sha256:adc4459d…` to `sha256:11861969…`; this release is built on the new base. The `vreckan` crate version is re-synced to 0.8.6 in `Cargo.toml`/`Cargo.lock`, which the nightly's `VERSION`-only bump had left at 0.8.5.
- **The default app store is renamed from "Vreckan Apps" to "SealSkin Apps".** A fresh install seeds the default store under the new name. On existing deployments, an idempotent startup migration runs on the first start after the upgrade: it renames the "Vreckan Apps" store row to "SealSkin Apps" (store names are unique per deployment, so that row is always the seeded default — even on hosts with a custom `VRECKAN_APP_RESOURCE_PATH`), renames the cached catalog file in place (under `/data/app_stores_cache`) so the store is served from its existing cache instead of being refetched from scratch, backfills the `source` of installed apps recorded against the store, and drops the orphaned `VRECKAN_SESSION_PORT` `app_settings` row. The migration logs once; a second start is a no-op (ANT-492).
- **`VRECKAN_SESSION_PORT` is removed.** The Rust port serves the session proxy on the same listener as the API, so the variable was metadata-only: it is no longer a defined setting (it also drops out of the admin settings response), and no `VRECKAN_*` environment variable with that name is read anymore. `VRECKAN_API_PORT` (the bind port) and the `api_port` metadata setting are unchanged (ANT-492; ANT-480).

## 2026-10-21 — F2: owner-visible participant list with per-joiner revoke and a 24-hour join-token TTL, v0.8.5 (ANT-467)

Finding F2 of the 0.8.5 review round closes the board-accepted v1 tradeoff recorded in v0.8.4 (ANT-444, F2): join grants were permanent and had no owner-side revoke path — the owner's only levers were killing or recreating the session.

- **Owners can see who is in their sessions.** Each session records every admitted visitor with how they got in; the Sessions view shows a participant-count badge on your own sessions, and the session's share dialog gains a **Participants** section: each person, how they got in (Join or a share link — with which link), when they joined, and for joiners, when their join access expires. Share-link visitors are listed by username and the link they used; they stay in while the link is active, so cutting them means revoking that link, exactly as before.
- **Per-joiner revoke.** Each joiner row in the Participants section has a two-step **Revoke** that removes that one person on their next request — the session, the other participants, and every share link are untouched, and a later Join starts them a fresh grant. This also fixes the underlying revoke path for share links: revoking a link now strips the visit grants that link minted even on plain (non-promoted) sessions, where the synthetic link id never reached the durable share store.
- **Join tokens get a 24-hour TTL.** Join access is granted for 24 hours from the last (re-)join: a joiner who keeps coming back stays in (each re-join after a lapse refreshes the window), one who is away more than a day has to Join again, and the background cleanup schedule expires lapsed grants without a restart. Grants issued before this release pick up a TTL on the joiner's next (re-)join. The window is a fixed 24 hours for 0.8.5; a per-session, owner-configurable TTL is a candidate follow-up.
- **Session lists carry the participant usernames** for the session's owner only (the admin cross-user view is unchanged), which is what the badge and the modal render from — no extra round trip.

## 2026-10-21 — v0.8.5: legacy (guacamole-lite) tier behind the session proxy + multi-user security deferrals (ANT-450; ANT-444 F1–F5; ANT-472)

The pinned pre-2.0 image (`ghcr.io/linuxserver/webtop:ubuntu-kde-2e6d5ead-ls2` — the guacamole-lite tier: `guacd` + `xrdp` + a node gclient) now works behind the session-prefix proxy: the desktop materializes for the owner and for joiners and share visitors, closing the Phase 4 QA findings F-P5 (systemic: no desktop for any role) and F-P6 (the desktop data plane was not token-gated).

- **The session proxy remaps the legacy tier's paths in both directions (F-P5).** The legacy gclient serves its page at the exact `/` route and references its assets root-absolute — the page's `/public/…` includes and the `/guaclite` tunnel dial in `rdp.js`. Behind the session-prefix proxy, the prefix-intact upstream leg 404ed on the raw image (the readiness check never passed without a one-line image shim) and the browser resolved the root-absolute references against the vreckan root (404 `text/plain`; strict MIME type checking refused to execute them as JS, so the guac client never booted and no X display ever spawned). For sessions labeled `selkies_capability: "legacy"` at launch, the proxy now strips the session prefix on the upstream leg (page at `/`, statics and the tunnel at their in-container paths) and rewrites the root-absolute `/public/…` and `/guaclite` references in the text responses it serves back to the session's proxy path. Secure-mode (selkies 2.x) sessions keep the prefix-intact, unrewritten behaviour, and the readiness gate accepts a ready legacy container (three consecutive subfolder-404 / root-200-HTML polls), so the raw pinned image needs no shim.
- **The desktop data plane is now token-gated (F-P6).** The legacy gclient itself validates no token — it renders the same encrypted RDP credentials into the page for every visitor — so before this fix the URL's `?token=`/`?access_token=` gated page access at the proxy but not the desktop. The tunnel dial now resolves through the session proxy, whose cookie/token gate applies to the page and the `/guaclite` tunnel alike. Residual hardening is tracked for the image side (the prefix-aware gclient build, F-P5 candidate (b)): per-visitor RDP credentials so a view-only share visitor cannot inject input.
- **Catalog note:** the board's open question of holding the pinned digest out of the catalog until F-P5 lands is resolved by this fix — the digest's sessions are functional again (still the labeled-legacy tier: no selkies secure mode, no WebRTC, no multi-user token table).
- **The session list no longer leaks other users' launch context (F3).** `GET /api/sessions` now nulls `launch_context` (the launched file/URL) and the custom session `name` for sessions the caller cannot join; own and joinable sessions stay fully populated (ANT-444 F3; ANT-468).
- **Joining another user's session is gated on the owner's express grant (ANT-472).** Since v0.8.4 any user with app access could self-serve Join another user's session; now a share link can name the invited user (`invited_username`), and another user's session appears on the apps page — and accepts `POST /api/sessions/{id}/join` — only when the owner has granted access: a non-revoked, non-expired named link, or a persisted participant entry (grandfathered v0.8.4 joiners stay until the owner revokes). Revoking or expiring an invited link strips the invitee's participant entry everywhere the join token lives, and the revocation response names the invitee.
- **Companion 0.8.5 changes land in the same PR:** the F1 no-store join hop (ANT-466), the F4 E2EE body gate on the join POST (ANT-469), the F4-followup E2EE body gate on session recreate + stop (ANT-471), the F5 `stop_session` map prune with generation-ordered token-table re-push (ANT-470), and the F2 participant list with per-joiner revoke and the 24-hour join-token TTL (ANT-467).

## 2026-10-21: Selkies multi-user (launch-time secure mode, first-class Join, pre-2.0 detection, multi-user docs), v0.8.4 (ANT-436; ANT-437/ANT-438/ANT-440/ANT-443/ANT-449)

v0.8.4 lands the Selkies multi-user phase (plan rev 20): plain webtop sessions enter Selkies secure mode at launch, a second user can join a running session with a single action, and the catalog knows which images support which Selkies features. The release ships with the multi-user usage docs (Phase 3, landing on release day).

- **Plain sessions enter Selkies secure mode at launch (ANT-438).** At launch, vreckan probes the container's Selkies build and promotes plain sessions to secure mode whenever the image supports it: the session's 256-bit master token gates the data plane, so two users on one session URL no longer kick each other. A Selkies 401 plain-text probe answer is treated as secure; an HTML answer stays ambiguous (`463793a3`/`558f3805`).
- **Owner WebRTC input works on the stock session URL (Phase 4 fix, ANT-449).** Selkies 2.0.0's WebRTC client sources its client token only from the page URL's `?token=` param, but the stock owner URL exposed only `?access_token=`, so on TURN-configured dual-mode images the signaling side rejected the owner's WebRTC HELLO and the owner's input was dropped. The owner session URL now carries the session's access token under both params (the same value), centralized in a single construction helper used by all owner-URL sites; no protocol, client, or image change (`32bbd297`).
- **First-class Join for a second user (ANT-440).** Running sessions get a self-serve Join action: a verified user with access to the session's app mints a 128-bit participant token (full input + file capability), recorded under the joiner's real username and idempotent across vreckan restarts and session recreates. The join-confirm modal states the observed websocket hand-off (`b6b93fdc`/`d79bb568`).
- **Owner control (MK) is never lost to a joiner or to the owner's own tabs (ANT-443).** Plain sessions re-derive control on data-plane connect/disconnect, granting it only to the most recently connected controller; a disconnected owner does not hold it and viewers never claim it, so the owner is never displaced by their own activity while a second user is joined (`699eae30`).
- **Pre-2.0 image detection + catalog capability labels (ANT-437).** The webtop catalog now carries per-image `selkiesVersion` records, and the launch-time capability probe classifies pre-2.0 images, so the catalog can label which images support multi-user secure mode (`968540ec`).
- **Multi-user docs (Phase 3).** The release is accompanied by the multi-user usage documentation (owner and second-user flows, join and kick semantics), landing on the planned release day.
- **Join-token v1 tradeoff (board-accepted; ANT-444 finding F2).** In v1, join tokens have no expiry and no owner-side revoke path: the owner's only levers are killing or recreating the session. Accepted as a design tradeoff for the first multi-user release; a participant list with per-joiner revoke and/or a join-token TTL is tracked for 0.8.5.

## 2026-10-20 — v0.8.3: trusted-peer gate on `X-Forwarded-Proto` + collab-room token hygiene, base-image bump (ANT-346; ANT-344)

A patch release cut on the nightly base bump: two HTTP-surface security fixes
landed on `main` between v0.8.2 and the tag, together with the docs/website
batch of that week. The image is the v0.8.2 source plus those changes, compiled
on the re-baked build base.

- **`X-Forwarded-Proto` is now honoured only from a configured trusted proxy (ANT-346).** Before this release any client could set (or omit) the header at will, so a client that reached the origin port directly — bypassing the proxy it was "behind" — could talk the session cookie out of its `Secure` attribute: `POST /api/auth/login` set `vreckan_auth` without `Secure` whenever the request claimed `X-Forwarded-Proto: http`. The scheme now goes through the same gate ANT-276 introduced for `X-Forwarded-For` in the login limiter: the header is accepted only when the **direct TCP peer** matches `VRECKAN_TRUSTED_PROXIES` (comma-separated IPs/CIDRs); otherwise the actual connection scheme applies, which is always `https` because the server terminates TLS itself. The gate covers the session cookie's `Secure` flag on login, logout, and self-revoke, and the OIDC `redirect_uri` derived from the request's base URL (including the admin SSO "test connection" path). Regression tests pin both directions (`test_secure_cookie_forged_xfp_from_untrusted_peer`, `test_secure_cookie_xfp_honoured_from_trusted_proxy`) plus unit coverage of the gate itself; the OIDC flow tests now issue from a configured trusted peer so their derived `http://testserver` base URL still resolves. **Operator action for TLS-terminating-proxy deployments:** set `VRECKAN_TRUSTED_PROXIES` to your proxy's address or CIDR (e.g. `10.0.0.0/8, 192.168.1.5`) if you need forwarded headers honoured — the empty default trusts no proxy, which is the safe posture for a directly-exposed origin, and it is an environment setting (no admin-UI field).
- **Collaboration-room token hygiene (ANT-344).** Three items from the same review. The room's viewer-disconnect log line printed the raw live viewer token; it now logs an 8-character prefix (char-boundary safe) — enough to tell two viewers apart in a log, too short to recover the credential. The visitor-invite redirect (the `303` whose `Location` carries a freshly minted per-visit token) now ships `Cache-Control: private, no-store`, so a caching proxy in front of the server cannot hand one visitor's token to another — the same class of fix as the v0.8.1 share-redirect hardening (ANT-322 F5). And every token comparison in the collaboration room and the session proxy moved to one shared constant-time helper (`src/tokens.rs`, length folded into the byte-walk accumulator so the runtime does not depend on where the first difference is), replacing the scattered per-module `==` compares; the same commit clears the `never_loop` clippy failure that had been breaking `cargo clippy --all-targets` on `main`.
- **Token-in-page risk documented, not silently carried.** The room page still embeds the live tokens in its page JS; the accepted-risk decision (board decision D1 under ANT-310: viewers trust the server operator, and the page ships a strict per-request nonce CSP so only operator-served same-origin scripts run) is now recorded at the code site, with the hardening path named — short-lived one-time join tokens exchanged at the websocket upgrade.
- **The build base advanced to the latest `vreckan-base-build` image.** The nightly scan detected the base-build image had moved and bumped the patch version: the `Dockerfile` build-stage pin and `.base-image-digest` move together (ANT-277 lockstep) from `sha256:472e4238…` to `sha256:adc4459d…`. The bump commit (`19fb2216`) moves only `VERSION`, `Dockerfile`, and `.base-image-digest` — not `Cargo.toml`/`Cargo.lock` — so the crate version still read 0.8.0 at this tag; it was not re-synced against `VERSION` until v0.8.5 (`4eec569e`).
- **Docs and website (ANT-335/ANT-343/ANT-344/ANT-358/ANT-359).** A page-by-page walkthrough of the web app (`/docs/app-pages`) added and wired into the site nav and footer. The docs/website freshness pass landed the missing v0.8.1 entry, refined the v0.8.0 entry's sweeper claim to the share-sweeper portion (the crypto-session sweeper is still an open gap, now stated in `port-status.md`), documented the MySQL backend in setup/README/FAQ, and refreshed the current-release references plus the missing share-session feature card. The ANT-346 setting help text gained the `X-Forwarded-Proto` wording above; `AGENTS.md` documents the daemon-side bind-mount form for build/dev containers (`/srv/paperclip/data/…` — the path the Docker daemon resolves, where the in-container `/paperclip/…` view stalled a core-build run); and the site image dropped its build-time `gpgv` apt pin ahead of the `ubuntu/nginx` re-publish that bakes the CVE-2025-68973 fix in. In the ACME suite, the `cert_status` read that could land on a pooled connection still holding a pre-write WAL snapshot now retries through a short settle, bounding that `test_cert_status_reads_from_db` flake (ANT-328).

_Entry backfilled on 2026-10-23 (ANT-512): v0.8.3 shipped without a changelog
entry — found by the weekly docs-freshness check (ANT-509). The content is
reconstructed from the shipped code at the `v0.8.3` tag and the driving issues,
not from the release notes._

## 2026-10-19 — Control-page auto-update fix + admin-UI hardening, multi-user file-permission discipline, v0.8.2 (ANT-345; ANT-347/ANT-348/ANT-349)

The "Control page stuck on Loading…" bug (ANT-347) is fixed on both ends:
the server no longer holds the auto-update job's write lock while sleeping
between ticks, and the admin UI now paints immediately, treating a slow or
failing `auto_update_status` endpoint as non-fatal. The release also ships
the multi-user file-permission discipline from finding B of the October
monthly security posture review (ANT-336) — minor severity, exploitable
only by other local users on the host, which is not the default v1
deployment.

- **The Control page no longer sticks on "Loading…" behind the auto-update check.** `GET /api/admin/apps/auto_update_status` reads the job's state under a read lock, but the background auto-update job held the matching **write** lock for the entire update interval (default one hour) while sleeping between ticks — so the endpoint stayed pending for almost the whole interval and the Control page's `Promise.all` kept the whole panel on "Loading…". The job now drops the write lock immediately after recording `last_check_at` / `next_check_at`, and the status endpoint — the only reader of that state — answers promptly again (reported in ANT-347; no API or schema change).
- **The admin UI no longer waits on the auto-update status to paint.** `renderApps()` now paints the installed-apps table and store cards from `installed`/`stores` first; `auto_update_status` is non-fatal and fills the status line and the enable/interval controls when it lands, falling back to the code defaults and an "Auto-update status unavailable" line while it is pending or on error. A late arrival never clobbers a value the admin saved in the meantime (ANT-349).
- **A "Checking for updates…" toast makes a slow status fetch visible.** While the `auto_update_status` request is merely slow (not failed), the Control page shows a sticky lower-right toast and keeps the status line neutral instead of claiming the status is unavailable; both clear the moment the response lands (ANT-349, board direction on ANT-347).
- **The live SQLite database is tightened to 0600 at boot.** When the server
  connects with a SQLite URL, the database file and its WAL/SHM sidecars are
  best-effort `chmod 0600`, so other local users can no longer read users,
  sessions, and auth tokens straight off disk. The tightening is best effort:
  a non-writable directory cannot fail boot.
- **Per-user upload directories are always tightened to 0700.** The
  per-upload directory created at upload initiation is now `chmod 0700`
  whether it was just created or already existed; previously the tightening
  only ran in the error branch, so a pre-existing directory kept its umask
  permissions and another local user could watch a user's in-flight uploads.
- **Private keys are created owner-only from the first byte.** The E2EE
  server key and the self-signed dev TLS key are now created with `O_CREAT`
  mode `0600` instead of being written at umask permissions and chmodded
  afterwards, closing a transient window another local user could race on a
  multi-user host (same fix class as ANT-274/ANT-275).

No behaviour change for single-user deployments: every tightening is
best-effort, so a read-only or non-writable directory degrades to a log line
instead of failing boot.

## 2026-10-19 — Share-session link hardening (no-store share redirect, browse-gated file root) and build-base bump, v0.8.1 (ANT-322)

A follow-up hardening release for the v0.8.0 share-session links: it fixes
the two findings (F5/F6) from the v0.8.0 review, plus the scheduled nightly
re-point of the pinned build base.

- **The share redirect is no longer cacheable.** The 303 that
  `GET /share/{id}` answers (into the app, with the fresh per-visit token in
  the target URL) now carries `Cache-Control: private, no-store`, so a
  caching proxy in front of the server cannot hand one visitor's token to
  another (F5; `src/session_shares.rs`).
- **The no-slash `api/files` root joins the browse gate.** The container
  301s the no-slash file API root to the slash-terminated listing; that
  redirect previously sat outside the session-proxy's capability check, so a
  share visitor without the file-browse cap saw the 301 instead of the
  browse 403. The root is now gated like any listing (F6;
  `src/session_proxy.rs`).
- **Regression tests** cover both fixes
  (`test_share_redirect_no_store_cache_header`,
  `test_noslash_files_root_browse_gate`).
- **Build-base bump.** The nightly re-pointed the pinned `vreckan-base-build`
  digest (rust:1.99-slim) in both `.base-image-digest` and the `Dockerfile`
  build-stage pin, per the digest convention in `docs/ops.md`.

## 2026-10-18 — Share-session links: full & view-only share links with per-link capabilities, password, expiry and revoke, plus the room-mode display fix, v0.8.0 (ANT-303; ANT-304/ANT-305/ANT-306/ANT-307)

You can now share a running app session with anyone — no Vreckan account
needed. Each running session gets a **Share** action: create a link with a
role (**Full** — they can use the app — or **View-only** — they can watch,
no input), per-capability file access (browse / download / upload), an
optional password, and an optional expiry; active links can be edited and
revoked individually, without stopping the session.

- **Managed share links.** Recipients open `/share/{id}` with no account;
  a password-protected link shows the password page first. Each visit mints
  a fresh per-visit token, so two people opening the same link hold
  different tokens, and revoking or expiring a link strips every token that
  link ever minted — one leaked link no longer lights up everything. A
  **full** link on a plain session lets the recipient drive the app; a
  **view-only** link refuses their input server-side (v1 scope: room and
  secure-mode sessions — on a plain legacy session the viewer role rides
  the `?role=` query parameter and the input refusal is enforced
  client-side, an accepted v1 limitation), and the file capability
  toggles are enforced by the session proxy on every file request
  (ANT-304/ANT-305/ANT-306).
- **Lazy secure-mode promotion.** The first share visit to a plain session
  promotes it to Selkies secure mode (a master token is minted and the
  token table is pushed into the container); on older base images without
  the token control plane the session stays legacy and the recipient's role
  rides a `?role=` query parameter instead, so share links degrade
  gracefully across app images (ANT-305).
- **Room-mode display fix.** Room invite links (participant and read-only
  viewer) now show the live display for invitees: the session proxy accepts
  the room's collaboration tokens — the controller token or a viewer's —
  over HTTP and WebSocket, instead of only the owner's session cookie.
  Stopping a room now notifies its viewers ("session ended") instead of
  leaving a dead iframe (ANT-304).
- **The expired-share sweeper is finally scheduled.** The share-cleanup job
  runs on the `share_cleanup_interval_seconds` schedule and covers both
  file shares and session shares, so expired links die on their own (the
  share-sweeper portion of port-status gap G2 — the gap itself stays open
  for the missing crypto-session sweeper; ANT-305).
- **Quick links stay documented.** The zero-setup options remain: the
  session URL itself (full access), `#shared` (server-enforced view-only),
  and `#player2`–`#player4` (gamepad co-play). The new site page
  "Sharing a session" documents all three sharing methods, the trust model
  (the server operator is in the middle of the app stream; E2EE covers the
  management API only), and the security notes (the link is a capability —
  send it over a trusted channel; the per-visit token sits in the URL only
  until the first redirect strips it) (ANT-308).
- **API docs.** `docs/api.md` and the site API reference document the new
  routes with request/response shapes: owner CRUD on
  `/api/sessions/{id}/shares` (E2EE) and the account-less
  `GET/POST /share/{id}` entry (ANT-308).
- **Website build-base bump.** The site's nginx runtime base moved from
  1.27 (alpine 3.21) to 1.30 (alpine 3.24) and is now pinned by the
  tag-level digest in `site/.base-image-digest` instead of the floating
  tag — clearing the nightly Trivy findings on the site image (ANT-315,
  following the ANT-277 digest convention).

## 2026-10-16 — Installed Apps image identity, scheduled auto-update, and the ANT-267 security batch, v0.7.0 (ANT-266/ANT-268/ANT-269/ANT-270 + ANT-273/ANT-274/ANT-275/ANT-276/ANT-277)

The Apps Control **Installed Apps** table now shows each app's real image
identity instead of "— unknown", and the update workflow (Check / Update /
scheduled auto-update) drives what the table displays. The release also
carries the ANT-267 weekly security batch.

- **The Image column stops saying "— unknown" after a server restart.** At
  startup the server inspects each installed app's local image (deduped,
  concurrently, best-effort) and caches its sha/digests, so the table shows
  the real image identity with no user action. A missing image or a down
  Docker daemon is logged and left unknown — startup never blocks, and the
  warm-up never touches pull bookkeeping (ANT-269).
- **"Check" and "Check all" now refresh the table.** The update-check
  endpoints persist the refreshed sha/digests — and the registry
  comparison — into the metadata cache the table renders from, so the
  result of a Check is visible in the list instead of vanishing (ANT-269).
- **The check toast reports the real image short id.**
  `get_local_image_info` returns the Docker-style 12-character `short_id`
  (image id minus its `sha256:` prefix); against a real daemon the Check
  toast's `current_sha` is no longer null (ANT-269).
- **Launch-time pulls are cached too.** When a launch pulls a missing image
  on demand, the pulled image's metadata is cached, so the table becomes
  accurate without admin action (ANT-269).
- **"Update available" is visible per app.** The check endpoints record
  `update_available` and `last_update_checked_at` per app; the list endpoint
  stays cache-only (both fields are `null` until a check has run — no live
  registry calls during a table render). The UI shows an amber Update button
  for apps with an available update and re-renders after a Check (ANT-270).
- **Scheduled update checks with optional auto-pull.** The previously
  unconsumed `auto_update_apps` / `auto_update_interval_seconds` settings now
  drive a background job: the first tick runs immediately at startup (local
  metadata warm-up plus a registry digest comparison), each tick re-reads
  the settings fresh (admin edits apply without a restart), the tick no-ops
  when auto-update is off, and with it on the job pulls opted-in apps that
  have an available update through the same background path as the manual
  Update button (live containers pick up the new image at their next
  start/recreate, as with a manual pull). A new read-only
  `GET /api/admin/apps/auto_update_status` exposes the job's state —
  `enabled`, `interval_seconds`, `last_check_at`, `next_check_at`,
  `updates_available`, `auto_update_apps_count` (ANT-269/ANT-270).

### Security (ANT-267 weekly batch)

- **Backups no longer carry live ACME/OIDC secrets** (major, ANT-273).
  `Db::dump` redacts the secret `app_settings` rows — the ACME account key,
  the Cloudflare token, the DNS provider config, the OIDC client secret —
  to a `<redacted>` placeholder, and `Db::restore` skips a placeholder row
  (older dumps with plaintext values still restore as-is). The ACME
  app-side cert dir now derives from the configurable data root
  (`{data_root}/acme`) instead of the hardcoded `/data/acme`, so moving
  `data_root` no longer silently loses the persisted account key and
  cert mirror on container recreation.
- **ACME key files land 0600 from the first byte** (major, ANT-274). The
  ACME account key is created owner-only (create-new with mode `0600`)
  instead of write-at-umask-then-chmod; replacing an existing file
  (e.g. one left world-readable by an older version) re-tightens it to
  `0600`, and a failed chmod is logged instead of swallowed. The on-disk
  cert/key mirror (`live/{domain}/cert.pem` + `privkey.pem`) uses the same
  owner-only writer, so the private key TLS serves no longer sits at the
  process umask.
- **The admin ACME surface stops leaking secrets** (minor, ANT-275).
  `GET /api/admin/certificates` masks `acme_dns_provider_config` (the raw
  JSON carries the provider's `api_token` / `client_secret` — blank on save
  keeps the stored value, like the Cloudflare token) and elides the
  embedded `status.key` private-key PEM for DB-sourced records. Custom CA
  directory URLs must use `https://` unless the new `acme_allow_http_ca`
  setting explicitly opts into plaintext; echoed acmex errors are capped at
  512 chars with the (possibly internal) CA URL masked; the `acmex` git
  fork is pinned to the `vreckan-2026-10-16` tag instead of a mutable branch
  rev; the self-signed dev key lands 0600 with an explicit 3-year validity
  and an expiry sidecar, so an expired dev cert is regenerated instead of
  being served forever.
- **The login limiter no longer trusts attacker-rotatable headers** (minor,
  ANT-276). The leftmost `X-Forwarded-For` entry is honoured for the
  per-IP login lockout only when the direct TCP peer matches the new
  `trusted_proxies` setting (comma-separated IPs/CIDRs; fail closed when
  empty); the lockout key lowercases the username so case variants of the
  same account share one budget; the still-on-default-password 401
  collapses to the generic "Invalid username or password." body under the
  default `warn` policy (the specific body remains only under an explicit
  `silent` opt-in); `save_sso_config` returns a generic 500 detail and logs
  the raw DB error. The clippy lint backlog across `src/` and the test
  suite is cleared in the same batch.
- **The Dockerfile build stage pins the base-build image by digest** (minor,
  ANT-277). The build stage no longer pulls a mutable
  `vreckan-base-build:latest`; the pin is recorded in
  `.base-image-digest`. The v0.7.0 image builds on the advanced base
  (digest `sha256:c4fa6f98750e…`); no operator action needed.

### Also in this release (merged to `main` since the v0.6.5 tag)

- **OIDC id-token verification adapted to jsonwebtoken v11** (ANT-246):
  the renovate bump of the `jsonwebtoken` crate lands with the signature
  changes the new major requires, so SSO sign-in is unaffected.
- Docs freshness pass: port-status test-parity notes (ANT-256) and the
  weekly doc/site corrections (ANT-242) ride the v0.7.0 image; the new
  repo `AGENTS.md` records the GitHub @-mention rule for agent comments
  (ANT-260).
- **The site image tracks `main` automatically.** A push to `main` that
  changes `site/` now dispatches the site build workflow, so the
  `vreckan-site` image in the Gitea registry stays current without a
  manual trigger (ANT-278).

## 2026-10-16 — Launch options move into the per-app Launch dialog; session name; first-sign-in password prompt, v0.6.5 (ANT-228/ANT-229/ANT-230/ANT-253)

The "Launch options" card at the top of the Apps view is gone. Picking an
app now opens a per-app **Launch dialog** that shows exactly the fields
that app type can use, and every launch option is set there, at launch
time. Verified against the T3 QA matrix (11 cases, live server).

- **One dialog for every app type.** Browser apps show a **Session name**
  field plus **URL to open** (required); file-capable apps show an
  **Open file on launch** section (upload a local file, or open a file
  already in shared storage — with an *Upload a file to shared storage*
  button when persistent storage is enabled); plain apps show the session
  name and launch options only.
- **Session name for browser sessions.** The optional session name
  appears in the session's window title and the active-sessions list, so
  a browser app pointed at different sites produces distinguishable
  sessions.
- **Moved controls.** Home directory (auto / cleanroom / a named home,
  with a *Manage* link to the home-directory list), language, GPU (shown
  only for GPU-capable apps on a server that reports GPUs), room mode,
  and Wayland (on by default) now live in the dialog instead of the
  top-bar card.
- **Presets are saved from and applied by the dialog.** *Save as preset*
  in the dialog footer saves the current options for that app; saved
  presets appear in the Pinned section of the Apps view (apply / make
  default / delete), and the dialog pre-fills from your default preset
  every time it opens (falling back to the admin's global default GPU).
- **Backend.** The applications payload now includes each app's store
  `type` (`app` / `browser` / `desktop` — data only, no client branching
  yet), and `file_path` launches honor room mode like every other launch
  path.
- **The three known issues found in the T3 QA matrix are fixed in the
  fix round and confirmed by the regression re-run** (5/5 pass, including
  both launch smoke paths — nothing previously green regressed; the
  re-run executed against this tree while it still carried the v0.6.4
  label, before the 0.6.5 bump in this entry). None of them remains a
  known issue for v0.6.5.
- **Wayland off now reaches the container env.** Unchecking Wayland in
  the Launch dialog sets `PIXELFLUX_WAYLAND=false` on the launch,
  overriding the `true` that linuxserver images bake into the image env
  (ANT-234).
- **The web client honors `must_change_password` at first sign-in.**
  While an account still carries its initial (bootstrap) password the
  server reports `must_change_password` in the login and `/me`
  responses; the client now opens the change-password dialog right after
  sign-in (form login or session restore) with a note explaining why.
  Closing it never traps the user — `VRECKAN_DEFAULT_PASSWORD_POLICY`
  still governs logins with the literal default password — and SSO
  accounts, whose password the identity provider manages, are skipped.
  This restores the long-documented "prompts for a new password at first
  sign-in" behavior, and the usage docs are aligned with the actual
  launch/Wayland/certificate behavior (ANT-253).
- **Explicit GPU selection no longer fails with a 500 in containerized
  deployments.** When the server's `/dev/dri` is bind-mounted at a
  different host path, the selected device is now remapped onto the real
  host path on the host side while the container keeps seeing the
  standard `/dev/dri` node, so an explicit pick launches cleanly
  (ANT-234).
- **Deleting your default preset resets the dialog.** After deleting your
  default preset, the next time the Launch dialog opens it restores the
  defaults: home `auto`, room off, Wayland on (language reverts to the
  browser guess, GPU to `Auto`) (ANT-235).
- **Build base advanced to digest
  sha256:1a5f145d0b719f9fefd939e6d56d46592e81a18a9d3606ba7f8ce19f7a15792d.**
  The `.base-image-digest` re-point landed on `main` after the v0.6.4 tag
  (stable `vreckan-base-build` base image, ANT-203), so the v0.6.5 image
  builds on the new base; no operator action needed (ANT-254).

## 2026-10-15 — rsa 0.10 crypto-stack unification, v0.6.4 (ANT-191)

`rsa` bumped to **0.10.0-rc.19** — the first release built on the
pkcs8 0.11 / spki 0.8 / der 0.8 / rand_core 0.10 / sha2 0.11 stack —
removing the `pkcs8_010` and `rand_core_06` version-alias shims that
v0.6.3 carried to keep `rsa` 0.9 compiling alongside the newer pins.

- **`rsa` 0.9 → 0.10.0-rc.19, `sha2` 0.10 → 0.11, `spki` 0.7 → 0.8.**
  All three move to the same RustCrypto major generation as `pkcs8`
  0.11 / `der` 0.8 / `rand_core` 0.10 already on main.
- **Alias shims removed.** The `pkcs8_010` and `rand_core_06`
  `[dependencies]` aliases (and every `use` of them) are gone; the
  code now uses `pkcs8` / `rand` directly.
- **RNG unified on `rand::rng()`.** One-time server-key generation and
  the PSS-nonce draw now use the thread-local CSPRNG (seeded from OS
  entropy) instead of the separate `rand_core 0.6` `OsRng`.
- **API adjustments for the new `num-bigint-dig` / `digest` 0.11
  types** (`to_bytes_be` → `to_be_bytes_trimmed_vartime`, turbofish
  moved to type position for `Pss` / `Oaep`).
- **`VERSION` and `Cargo.toml` bumped to 0.6.4.**

## 2026-10-15 — pkcs8 0.11 + rand_core 0.10, v0.6.3 (ANT-128)

The two RustCrypto ecosystem bumps deferred in v0.6.2 (while `rsa` 0.10
was still pre-release) have now landed as separate renovate PRs. Each
keeps a backward-compatible version alias so the still-unbumped `rsa` 0.9
crate keeps compiling; no operator-visible behavior changes, and the full
412-test suite passes on both branches.

- **`pkcs8` 0.10 → 0.11** (PR #27). A `pkcs8_010` alias on `pkcs8` 0.10
  preserves the `rsa` 0.9 / `pkcs1` 0.7 trait interop; it is dropped when
  `rsa` 0.10 (which consumes `pkcs8` 0.11 directly) is released.
- **`rand_core` 0.6 → 0.10** (PR #28). A `rand_core_06` alias on `rand_core`
  0.6 preserves the `rsa` 0.9 `OsRng` interop; same exit condition.
- **`VERSION` and `Cargo.toml` bumped to 0.6.3.**

## 2026-10-15 — ACME account key moved to ECDSA P-256; acmex pinned to upstream (ANT-138)

Let's Encrypt production stopped accepting **EdDSA** (Ed25519) account
keys in October 2026 — it now accepts only RS256 / ES256 / ES384 /
ES512. The published `acmex` 0.8.0 hard-codes the account key as Ed25519,
so `Issue/Renew now` failed with a misleading "Missing location header"
(the account `newAccount` POST returned 400 before the header check).

- **ECDSA P-256 account key (ES256).** `load_or_create_account_key` now
  generates P-256 keys and **rotates** any persisted key of another type
  (the orphaned account is simply re-registered under the new key).
  ES256 is accepted by all three supported CAs.
- **`acmex` pinned to the project's `vreckan-project/acmex` fork** of the
  upstream repo (relocated out of the maintainer's personal `antitux/acmex`
  account, ANT-284)
  (houseme/acmex `f7b06c2`, 2026-10-06, + six fixes) via
  `[patch.crates-io]`: only upstream derives the JWS `alg` and JWK from
  the actual key (`Jwk::for_key_pair`, `JwsSigner::jwa_algorithm`) and
  ships `KeyPairGenerator`. The fork additionally (1) defaults the
  `contact` field in the account response — Boulder omits it for
  contact-less accounts, so a successfully registered account (2xx +
  Location) still failed to parse — (2) sends an **empty payload** on
  POST-as-GET requests (order/authz lookup, challenge trigger,
  certificate download, account lookup): Boulder rejects the `e30`
  (`{}`) encoding with "POST-as-GET requests must have an empty
  payload", which 400ed the authorization fetch, and (3) budgets
  **5 minutes per order-polling stage** (the old 60s was below Let's
  Encrypt's dns-01 validation cadence — the first validation attempt
  fails while the DNS change propagates and retries arrive minutes
  later, so a correctly configured issuance timed out with the order
  still pending) — (4) activates dns-01 challenges with an **empty
  object** (`{`) instead of an empty payload: per RFC 8555 §7.5.1 (and
  Boulder's `postChallenge`), an empty payload is a POST-as-GET that
  only *fetches* the challenge state, so the fork's uniform empty
  payload turned every activation into a no-op fetch and the
  challenges sat "pending" forever — the single most insidious layer,
  because the order looked healthy (authz pending, order pending, no
  errors) and Let's Encrypt's own status page said "Operational" — and
  (5) sets the **CSR subject CN** to the first identifier: rcgen's
  `CertificateParams::default()` subjects every CSR to
  `CN="rcgen self signed cert"`, which Boulder validates as a domain
  name and rejects at finalization ("Domain name contains an invalid
  character") — and (6) makes the **Cloudflare TXT create idempotent**:
  a *new* order for the same domain reuses the *existing* valid
  authorization, so the challenge token (and thus the digest content)
  is unchanged and Cloudflare's 81058 "An identical record already
  exists" fires when a previous run left the record in place; the
  provider now reuses the existing record's ID. The re-resolution also
  moved the acmex DNS/HTTP
  subtree to the advisory-fixed majors (h2 0.4, hickory 0.26,
  rustls 0.23, rustls-webpki 0.103, hyper 1.x), so the now-unnecessary
  RUSTSEC ignores were dropped from `deny.toml` and the acmex git source
  was added to its source allowlist.
- **CA directory URLs resolve through acmex's CA table** instead of
  hand-coded constants that had gone stale: ZeroSSL now targets its
  current EAB-gated `https://acme.zerossl.com/v2/DV90` endpoint (the
  old `/v2/server/directory` 403s) and Google targets
  `https://dv.google.com/acme/directory` (the old `acme.v1.pki.goog`
  404s). ZeroSSL additionally requires an External Account Binding
  (account key ID + MAC key), which vreckan does not yet configure —
  tracked as a follow-up.
- **Verified end-to-end on the staging box (192.168.103.225,
  `vreckan.antitux.net`, Cloudflare DNS-01, Let's Encrypt
  **production**).** Boot-time issuance and the admin "Issue/Renew
  now" button both complete in ~2 s: a real `Let's Encrypt YE1`
  certificate for `CN=vreckan.antitux.net` (SAN included, 90-day
  validity) is saved to the cert DB, hot-reloaded into the serving
  TLS config, and presented on `https://vreckan.antitux.net`.
  The earlier "pending forever" symptom was *not* a Cloudflare,
  DNSSEC (the `antitux.net` zone has no DS at `.net`), or
  Let's Encrypt incident — it was the activation semantics above,
  confirmed by activating a stale challenge by hand (validated in
  <15 s) and by reading Boulder's WFE source.

## 2026-10-15 — Native ACME client, certbot sidecar removed (ANT-128)

The certificate-issuance path is rewritten around the native **`acmex`**
Rust ACME client. The legacy `certbot/dns-cloudflare` Docker sidecar
(`src/le_certbot.rs`) is retired — `acmex` is now the **only** issuance
backend, and the Docker socket is no longer required for certificate
issuance (it remains in use for app containers and GPU setup).

- **`le_*` → `acme_*` rename.** Every configuration key, API field, and
  internal identifier that carried the legacy "Let's Encrypt" prefix is
  renamed to the generic `acme_` prefix (13 DB settings, the admin API
  request/response shapes, env vars, and the UI i18n keys). The feature
  is now documented as **ACME** rather than "Let's Encrypt".
- **Three CAs.** `acme_ca` selects the certificate authority:
  **Let's Encrypt** (default), **ZeroSSL**, or **Google Public
  Certificates**. The `acme_staging` flag still selects the Let's Encrypt
  staging endpoint for test certificates.
- **Eleven DNS-01 providers.** `acme_dns_provider` selects the DNS
  provider that manages the domain's zone: Cloudflare (default), AWS
  Route 53, DigitalOcean, Linode, Azure DNS, Google Cloud DNS, Alibaba
  Cloud, GoDaddy, Tencent Cloud, Huawei Cloud, and CloudNS.
  `acme_dns_provider_config` carries the JSON credentials for any
  non-Cloudflare provider; `acme_cloudflare_zone_id` optionally pins the
  Cloudflare zone.
- **Certificate stored in the config database.** The issued bundle
  (PEM chain + private key + expiry + CA + domains) is persisted in the
  `app_settings` table as the source of truth. A best-effort file mirror
  is written to `/data/acme/live/<primary>/` for backward compatibility;
  the old `/data/le/` certbot layout is retired.
- **Force-renew button.** `POST /api/admin/certificates/renew` now always
  (re)issues, even when a valid certificate already exists (the previous
  "already valid" short-circuit that made the button appear to do
  nothing is gone). The response carries `issued`/`renewed` plus
  human-readable `logs` and `error` fields so the UI can explain what
  happened.
- **Dynamic hot-reload.** After a successful issuance or renewal the TLS
  endpoint is reloaded in-place — no server restart is needed. The
  background renewal loop reads the effective config fresh on each
  iteration, so changes made on the admin page take effect without a
  restart.
- **Docker build-deps for acmex.** The `Dockerfile` build stage adds
  `cmake`, `make`, and `golang` (required by the FIPS variant of
  aws-lc, which acmex pulls in via `aws-lc-rs/fips` and builds from
  source through its CMake project). See the per-package justifications
  in the `Dockerfile`.

**Upgrading:** pull the new image and `docker compose up -d`. The old
`VRECKAN_LE_*` env vars are **not** read by the Rust server — set the
`VRECKAN_ACME_*` names instead, and re-save any settings you had changed
from the admin Certificates page so they land under the new keys (rows
stored under the old names are not auto-migrated). The `/data/le/`
directory is no longer used; the cert mirror lives at `/data/acme/`.
(ANT-242, 2026-10-16: re-checked the current tree — the originally claimed
`VRECKAN_LE_*`→`VRECKAN_ACME_*` env fallback and `le_*`→`acme_*` key
migration are not implemented; flagged to the owner for a follow-up fix
or doc correction.)

## 2026-10-14 — Dependency bumps, v0.6.2 (ANT-118)

Batch dependency bump and version release. All bumps were QA-validated for
breaking changes before landing; no operator-visible behavior changes.
`VERSION` and `Cargo.toml` bumped to 0.6.2.

- **`aes-gcm` 0.10 → 0.11** (merged separately as PR #23 before this batch).
  The E2EE `Aes256Gcm` handshake API is unchanged; the removed 0.10-only items
  (`std`/`stream` features, `C_MAX`, `AeadInPlace`) are unreferenced in our code.
- **`axum-extra` 0.10 → 0.12.** We enable only the `cookie` feature; no
  `axum_extra` import exists in our Rust source (cookies are parsed/built
  manually in `src/auth.rs`), so the 0.12 breaking changes (opt-in features,
  `option_layer` body mapping, `async-stream` removal) do not apply.
- **`axum-server` 0.7 → 0.8.** The `tls_rustls::RustlsConfig::from_pem` and
  `bind_rustls` / `.serve(...)` call sites in `src/main.rs` are unchanged.
- **`base64` 0.22 → 0.23.** The `base64::engine::general_purpose` API
  (`STANDARD.encode/decode`, `URL_SAFE_NO_PAD.encode/decode`) is unchanged.
- **`reqwest` 0.12 → 0.13.** The feature flag name changed
  (`rustls-tls` → `rustls`); the `form` feature was added explicitly.
  The `default-features = false` + named-feature pattern is otherwise
  unchanged.
- **`rand` 0.9 → 0.10.** In 0.10 the `RngCore` trait was split: `fill_bytes`
  moved to a new `RngExt` sub-trait, and the unified `Rng` trait now
  re-exports both. All seven `use rand::RngCore` imports were updated to
  `use rand::Rng`; one additional `use rand::RngExt` was added where
  `fill_bytes` is called on the result of `rand::rng()`.
- **`der` 0.7 → 0.8.** Part of the RustCrypto ecosystem; the `der::Decode`
  import in `src/le_certbot.rs` is unchanged.
- **`x509-cert` 0.2 → 0.3.** The public-field access pattern
  (`cert.tbs_certificate.validity.not_after`) changed to method calls
  (`cert.tbs_certificate().validity().not_after()`); the single call site in
  `src/le_certbot.rs` was updated accordingly.
- **`rcgen` 0.13 → 0.14.** Used for the self-signed cert fallback in the
  Let's Encrypt / certbot flow; the API we call is unchanged.

**Deferred** (awaiting ecosystem releases): `rsa` 0.10 (currently pre-release
`0.10.0-rc.19`), which would in turn unblock `pkcs8` 0.11, `spki` 0.8,
`sha2` 0.11, and `der` 0.8 → 0.9. `jsonwebtoken` 10 → 11 is blocked on the
`jwks` crate ecosystem.

## 2026-10-14 — Post-release security fixes, v0.6.1 (ANT-80/81/87/88/89/90)

Post-release hardening of the v0.6.0 switchover, from the ANT-45 security
review. All eight fixes are merged to `main` and verified by QA (P1 wave on
ANT-82, P2 wave + full regression on ANT-91); no functional regressions. The
P3 findings from the review are deferred to a later release. `VERSION` and
`Cargo.toml` bumped to 0.6.1.

- **Non-root container (P1-1, ANT-81).** The server process now runs as a
  dedicated non-root user (`vreckan`, UID/GID 1000); the default
  `docker-compose.yml` drops `privileged: true`. It still binds 443 (via a
  `cap_net_bind_service` file capability on the binary) and reaches the host
  Docker daemon through the host `docker` group instead of blanket
  privileges. A one-shot `data-perms` service chowns the `vreckan-data`
  volume to UID/GID 1000 on every `docker compose up`, so no manual `chown`
  is needed on fresh installs or when upgrading from v0.5.x. Set
  `VRECKAN_DOCKER_GROUP` if your daemon's socket group is not named `docker`.
- **First-boot admin must change password (P1-2, ANT-80).** The bootstrap
  `admin` account is now flagged *must-change-password*, so the web client
  prompts for a new password at first sign-in. The first-run log no longer
  prints a *configured* bootstrap password (only the well-known `admin1234`
  default). A new `VRECKAN_DEFAULT_PASSWORD_POLICY` (`allow` | `warn`
  (default) | `reject`) controls how a login with the literal `admin1234` is
  treated while that flag is still set.
- **E2EE server key persisted (P2-1, ANT-87).** The E2EE server RSA key now
  defaults to the persistent data root — `VRECKAN_SERVER_PRIVATE_KEY_PATH` is
  `/data/ssl/server_key.pem`, written `0600` — instead of the ephemeral
  `/config/ssl/`. It now survives container recreation; previously a
  re-created container generated a fresh key and forced every client to
  re-pair.
- **Cloudflare ACME token written 0600 (P2-2, ANT-87).** The Cloudflare
  DNS-01 token file the server writes for certbot
  (`/data/le/cloudflare.ini`) is now tightened to `0600`, so the plaintext
  token is owner read/write only.
- **Migration script masks DSN credentials (P2-3, ANT-88).**
  `scripts/migrate-sqlite-to-postgres.py` now masks the `user:pass` userinfo
  when it prints the target DSN and scrubs it out of connection-error
  tracebacks, so credentials no longer leak into logs.
- **Backup hygiene (P2-4, ANT-88).** Config-DB backup archives are written
  `0600`; the in-archive database dump excludes live session tokens
  (`auth_tokens`) so a restore leaves in-flight logins untouched; and the
  default archive retention changes from keep-all to 7
  (`VRECKAN_BACKUP_RETENTION`).
- **Login rate limiting (P2-5, ANT-89).** `POST /api/auth/login` now
  rate-limits per (client IP, username): `VRECKAN_LOGIN_MAX_ATTEMPTS`
  (default 5) failures within `VRECKAN_LOGIN_LOCKOUT_SECONDS` (default 900 s)
  lock the pair out with HTTP 429 for the same window (fail closed). A
  successful login clears the pair; `0` disables the limiter.
- **Postgres loopback-only + default-creds guard (P2-6, ANT-90).** The
  bundled Postgres service is now published on the host loopback interface
  only by default (`127.0.0.1:5432`) — the server reaches it over the
  internal compose network either way; set `VRECKAN_POSTGRES_PORT=5432` to
  expose it on every interface again. Starting against the default
  `vreckan`/`vreckan` credentials logs a warning by default;
  `VRECKAN_ALLOW_DEFAULT_DB_CREDS` (`1` to silence, `refuse` to fail startup)
  controls the guard.

**Upgrading:** pull the new image and `docker compose up -d` — the
`data-perms` service handles ownership automatically. If you rely on the old
`/config/ssl` E2EE key location or the previous SQLite-as-compose-default
setup, review [docs/ops.md](./docs/ops.md) and the
[configuration reference](./README.md).
- **Core Rust fixes from the `ant-30` branch (ANT-30) merged into the
  v0.6.0 tree.** Swallowed database-write errors in the global-settings,
  SSO-config, and template-schema saves now surface as 500 instead of
  silently reporting success; E2EE requests with a mis-sized IV get a 400
  instead of an index panic; and the files listing validates its pagination
  bounds (400) instead of indexing out of range.
- **Python-era leftovers removed from the tree.** The root
  `requirements.txt` and `pytest.ini` belonged to the FastAPI server (now in
  the `vreckan-python` archive repo) and the old pytest suite (now a Rust
  integration test), so both are gone. `server/` stays: the image ships
  `server/static` (web client) and `server/template_schema.yml` (template
  editor baseline). The "no Python in the image" claim now matches the tree.
- **`scripts/migrate-sqlite-to-postgres.py` finalized** (it is the only
  Python left in the repo). Its table order now matches `db.rs`
  `TABLE_ORDER`, and its generated DDL now uses the exact column types the
  server's own schema sync creates on a fresh Postgres (`varchar`/`json`,
  not `text`/`jsonb`), so a migrated database boots with no schema-drift
  warnings. It remains idempotent (safe to re-run), and `--dry-run` still
  previews the DDL and row counts with the stdlib alone. A real run needs
  `psycopg2-binary` — see docs/ops.md §4.1. Verified end-to-end against a
  live `postgres:16` with a real v0.6.0 SQLite database.

## 2026-10-13 — Python-to-Rust switchover, v0.6.0 (ANT-40)

- **v0.6.0 release.** `VERSION` and `Cargo.toml` bumped to 0.6.0; the
  switchover cuts the deployment over from the Python/FastAPI server to the
  Rust (axum/SeaORM) server.
- **Postgres is now the default deployment database.** `docker-compose.yml`
  bundles a `postgres:16` service and points the server at it by default via
  `VRECKAN_DATABASE_URL`; the service is health-gated so the server only
  starts once it is ready. Override in `.env` to use your own Postgres or to
  fall back to SQLite. See [docs/ops.md §4](./docs/ops.md).
- **SQLite → Postgres migration tool.** New `scripts/migrate-sqlite-to-postgres.py`
  moves an existing SQLite configuration database onto a Postgres database so
  a running deployment can be cut over without re-seeding users and settings.
  Idempotent (safe to re-run); `--dry-run` previews the DDL and row counts
  without touching the target.
- **Repo switchover.** The Rust port becomes the primary code in `vreckan`;
  the legacy Python server moves to the `vreckan-python` archive repo (its
  GitHub Actions workflows are disabled there). Only the web client
  (`server/static/`) and the bundled template schema
  (`server/template_schema.yml`) remain of the old tree; the Python-era
  root files `pytest.ini` / `requirements.txt` no longer exist (the one
  remaining Python file is the migration tool in `scripts/`).
- **Final Rust deployment image + CI/CD (ANT-41/ANT-43/ANT-57).** The
  `Dockerfile` is multi-stage: a `rust:1.99-slim` build stage (sccache-wrapped
  `rustc`, BuildKit cache mounts), a test stage that pre-compiles
  `cargo test --profile test-release`, and a `debian:trixie-slim` runtime
  (no Python in the image). CI is the GitHub-light/Gitea-heavy pattern:
  `gitea-mirror-trigger.yml` mirrors main/PRs to the Gitea mirror and
  dispatches `build-test-rust.yml` on the self-hosted runner; the `v0.6.0`
  tag push fires the Gitea Release workflow, which publishes
  `ghcr.io/vreckan-project/vreckan:v0.6.0` alongside `:latest` (and to the
  Gitea registry). See [docs/ops.md §3](./docs/ops.md).
- **Docs finalized for the v0.6.0 state (ANT-47).** `README.md`,
  `docs/ops.md`, the `site/docs/` project-site pages, and the
  `HISTORY.md` cross-references were reconciled with the final switchover
  state (Postgres-default compose, final CI/CD flow, the `vreckan-python`
  archive); `docs/api.md`, `docs/release.md`, and `docs/port-status.md`
  were checked and needed no changes.

## 2026-10-12 — Rust port kickoff (ANT-1)

- **Initial Rust port of the server** — the Python/FastAPI/uvicorn server is
  rewritten as a single static Rust binary (`vreckan`; axum + tokio, SeaORM on
  SQLite/PostgreSQL/MySQL, `ring` for the E2EE crypto, `bollard` for the
  Docker provider, `tokio-tungstenite` for the WebSocket legs). All 103 unique
  endpoint paths of the original are present (one additive `GET /api/health`),
  the web client under `server/static/` is carried over byte-identical, and
  the test suite is ported 1:1 by name (387 of 388 original tests; 10
  Rust-only additions).
- **Parity audit (ANT-5)** — full static-analysis parity audit against the
  Python original (Gitea baseline `e7b2de6`, v0.5.0): route tables
  machine-extracted and diffed, handler behavior verified. Result: complete
  route coverage, 11 documented gaps (G1–G11, 3× P1) handed to core dev. The
  gap list is maintained in [`docs/port-status.md`](./docs/port-status.md).
- **Multi-stage Docker image + compose (ANT-3)** — `rust:1.99-slim` build
  stage, `debian:trixie-slim` runtime (no Python in the image);
  `docker-compose.yml` for the Rust image (TLS 443, `vreckan-data` bind
  mount, Docker socket, `/dev/dri` + group add, optional Postgres service);
  `.base-image-digest` pins the build base image for the nightly-scan digest
  flow.
- **Documentation (ANT-8)** — README updated for the Rust port; new
  [`docs/`](./docs/) tree: `api.md` (endpoint reference with auth/E2EE
  requirements), `ops.md` (deployment, registries, Gitea runner/CI, databases,
  TLS, data layout), `release.md` (release & changelog process), and
  `port-status.md` (known-gaps living record).

## 2026-09-24 / 2026-09-25 — Branding, UI polish, DAST hardening
- **Vreckan wordmark** in the sidebar and on the login page (replaces the
  icon + name); the login tagline is gone; the "or" divider between the
  password and SSO buttons is styled. The full logo kit (wordmark, icon,
  PNG sizes) now ships in `static/img/`.
- **Admin UI polish** — Installed Apps shows the installed-apps table above
  the install-from-store card; the sidebar reads "Control" / "Stores";
  Templates and Laboratory carry red BETA tags (sidebar + card titles); the
  Advanced settings card gets an amber ADVANCED tag, a "changes can break
  app launches" warning, and a link to the Backup page; the group add/edit
  forms drop the vestigial group dropdown. All new/changed strings
  translated into all 18 languages.
- **Security headers on every response** (middleware in `api.py`): CSP,
  HSTS, `X-Content-Type-Options`, `X-Frame-Options: SAMEORIGIN`,
  Permissions-Policy, COEP, COOP, CORP.
- **Strict CSP (Phase 2)** — the default CSP carries no `'unsafe-inline'`:
  the main app's 95 inline `style=""` attributes became utility classes in
  `app.css`; the collaboration room and the public password page use a
  per-response nonce for their inline script/style; the reverse-proxied
  session page (third-party noVNC/app HTML) keeps a looser CSP with
  `'unsafe-inline'`.
- **`requirements.txt`**: `sqlalchemy` → `sqlalchemy[asyncio]` (pulls in
  greenlet, which the dev container was missing and which crash-looped the
  server on recreate).
- **ZAP full-scan clean: 0 High / 0 Medium / 0 Low** (8 informational noise
  items remain). Test suite now **345 tests / 20 files**.

## 2026-09-23 — RBAC, SSO config page, bootstrap admin, directory reorg
- **RBAC rewrite** — a 17-permission catalog in `server/permissions.py`
  (12 admin + 5 user, plus the `admin` super-permission); effective
  permissions are the union of direct grants, roles, and groups; `is_admin`
  is derived from the `admin` permission. Per-endpoint permission gating in
  the routers.
- **SSO group re-stamp gating** — on each SSO login, only groups present in
  the configured admin/user group lists get their roles re-stamped (prevents
  IdP groups from leaking roles). Group cards on the Groups page are now
  editable in place.
- **SSO config page** — SSO settings moved to a DB-over-env model: the
  `app_settings` table wins over the environment, with per-field source
  badges (db/env/default). New admin **SSO page** (connection + group
  mapping, masked client secret, tag inputs, live connection test) behind a
  new `admin.sso` permission (18th in the catalog). Endpoints:
  `GET/PUT /api/admin/sso`, `POST /api/admin/sso/test`. All `VRECKAN_OIDC_*`
  env vars are now optional (commented out in `.env`; the app runs purely
  from the DB).
- **Configurable bootstrap admin** — `VRECKAN_BOOTSTRAP_ADMIN_USERNAME` /
  `VRECKAN_BOOTSTRAP_ADMIN_PASSWORD` (defaults `admin` / `admin1234`), read
  at seed time; the delete/demote/role-strip protection guards resolve the
  bootstrap username dynamically (DB first, then env).
- **Directory reorganization** — everything under `server/` moved up one
  level: the app package `server/app/` is now `server/` (imports renamed
  `app.*` → `server.*`; uvicorn target `server.api:api_app`), and the deploy
  files (`Dockerfile`, `docker-compose*.yml`, `run_https.py`,
  `requirements.txt`, `pytest.ini`, `.env`, `.env.example`, `.dockerignore`,
  `dev-container.sh`) plus `tests/` now live at the repository root. The
  `vreckan-data` bind mount is now `./vreckan-data`.
- **`.gitignore` rewritten in explicit-ignore style** (junk listed; everything
  else tracked by default) — replacing the old `*/` whitelist in
  `server/.gitignore`, which had silently left `app/routers/`,
  `app/static/{js,css,img}/`, and `tests/` untracked.
- **i18n re-added to the SPA** — the 18-language dictionaries from the
  upstream client were restored to `server/static/i18n/` (the fork had dropped
  them when the esbuild client was replaced). A small runtime
  (`server/static/js/i18n.js`) loads the dictionaries, deep-merges each
  language over English (the non-English files are partial), and exposes
  `t()` with ICU-style plurals + placeholders, `applyTranslations()`, and
  `setLanguage()`. The static DOM is instrumented with `data-i18n`
  attributes, and a **language selector** in the sidebar footer switches the
  UI live (persisted to `localStorage`, honored on the next load; the room
  page keeps its own self-contained `translation.js`). The new
  Vreckan-specific keys (login/nav/launch/files/password/share/common) were
  translated into all 18 languages.
- **Test suite now 342 tests / 20 files**, all green in the dev container.

## 2026-09-21 / 2026-09-22 — ecdsa (Minerva) resolution
- **Verified the ecdsa Minerva timing-attack finding unreachable** at runtime
  (jose uses the cryptography backend for EC whenever cryptography is
  installed; the live IdP signs RS256/RSA).
- **Removed `ecdsa` from the image** (`pip uninstall` in the Dockerfile + dev
  container) to keep security scanners quiet.

## 2026-09-20 — Docker lifecycle tests, router split, base-image update
- **Tier 5 test coverage** — real Docker lifecycle tests.
- **`api.py` split** — the 5,652-line monolith was split back into 8 router
  modules under `app/routers/` (`admin`, `auth`, `encrypted`, `files`,
  `homedir`, `pinned`, `session`, `upload`); `api.py` shrank to ~3,135 lines.
- **Package-update pass** — dev base image advanced to debian 13.7; security
  re-scan clean.

## 2026-09-19 — Test coverage Tiers 2–4
- **Tier 2** — people, volume mounts. **Tier 3** — admin misc, user sessions,
  user homedirs. **Tier 4** — uploads + files.

## 2026-09-18 — Test hygiene + HTTP client
- **Test-warning cleanup** (all 29 pytest warnings resolved) and the **httpx →
  httpx2 migration** (the app's HTTP client; `httpx` also uninstalled from the
  live container).
- **Tier 1 test coverage** — OIDC + backup/restore.

## 2026-09-16 — Templates, launch UX, first test suite
- **Template settings separated from global settings** (per-app overrides).
- **Template schema default/current split** — push-through only when the admin
  changed a value from the image baseline.
- **Launch modal file-toggle** for file-backed apps; **optional session naming
  at launch** (`TITLE="<App> - <Name>"` env override, creation-time only).
- **First test suite** — `tests/` (6 files, 46 tests) + `pytest.ini`.
- **Settings audit + SealSkin→Vreckan rebrand** — 12 vestigial settings
  deleted; naming swept across the codebase.

## 2026-09-15 — Deployment + auth groundwork
- **Migrated to `0.0.0.0:443` + Let's Encrypt + the real hostname**
  (`vreckan.antitux.net`); the OIDC redirect URI is derived per-request from
  the Host header, so SSO works on any access path.
- **Admin/user unification** — a single people API; an admin is a user with
  `is_admin=true`. Added the `users.is_sso` column (SSO users have no local
  web password); the two SSO test accounts were flagged manually.
