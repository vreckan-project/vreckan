# Vreckan (Rust) — Release & Changelog Process

How releases are cut and how the changelog is maintained. The release
mechanics are the `bump-and-tag` / `.base-image-digest` flow from
[ops.md §3](./ops.md); this document is the **process contract** for the
release and documentation owners.

## Roles

| Role | Owns |
|---|---|
| **Release (Roberta)** | Cutting releases: the `VERSION` bump, the `bump-and-tag` run, and the resulting image tags in the registries |
| **Documentation (Will, for the time being)** | The per-release entry in `CHANGELOG.md`, the release notes, and keeping [port-status.md](./port-status.md) current with the shipped fixes |

The two coordinate per release: the changelog entry is **authored before the
tag is cut** (it ships in the same commit as the `VERSION` bump), so the
release notes and the image tag always agree.

## The flow

1. **Prepare on `main`.** All changes for the release are merged to `main`.
   The Documentation owner writes the `CHANGELOG.md` entry for the release
   (convention below) and bumps `VERSION` in the same branch:
   ```bash
   ./scripts/bump-version.sh minor   # or: major | patch (default: patch)
   ```
   `scripts/bump-version.sh` is the single source of version math, shared by
   the GitHub and Gitea-side workflows. Commit both changes together and push
   to `main`.
2. **`bump-and-tag` fires** (GitHub Actions, on push to `main`): it reads
   `VERSION`; if tag `v<VERSION>` does not exist yet, it mints a short-lived
   GitHub App installation token, creates and pushes the annotated tag
   `v<VERSION>`, and mirrors the tag (plus the commit as `main`) to the Gitea
   mirror repo.
3. **Gitea Release workflow** (in the `vreckan/vreckan-workflow` repo) fires
   on the tag push to the mirror: builds the image on the self-hosted runner
   and pushes it to **GHCR** (`ghcr.io/vreckan-project/vreckan-rust`) and the
   **Gitea registry**, with the versioned tag and `latest`.
4. **Post-release check.** Verify the tag exists on GitHub, the image tags
   are present in the registry, and the compose file's
   `ghcr.io/...:latest` reference resolves. If anything slipped, cut a
   follow-up patch release (same flow, `patch` bump).
5. **Base-image bumps.** When the nightly scan bot re-points
   `.base-image-digest` (build base digest drifted), the resulting image
   rebuild is noted in the next changelog entry ("build base advanced to
   digest …") so operators can see it in release notes.

## Changelog convention

- **File:** `CHANGELOG.md` at the repo root, **newest entry first**.
- **Headings:** `## <date> — <release theme>`, e.g.
  `## 2026-10-12 — Rust port kickoff`. One section per release (or per
  coherent work batch when the release cadence is looser); the existing
  Python-era entries are the style reference.
- **Entries:** bullet points, bold lead phrase, then detail. Each bullet says
  *what changed and why it matters to an operator or user*, not an internal
  narrative. Reference the driving issue where it helps (`(ANT-3)`).
- **Scope:** user-visible behavior, configuration, deployment, security, and
  anything an operator should know when upgrading. Pure internal refactors
  are optional but welcome when they affect behavior (e.g. a moved default).
- **Rust-port era:** entries from the Rust port onward describe the Rust
  server. The Python-era history stays as-is below them; the fork's
  pre-fork history lives in `HISTORY.md`.
- The changelog is the **authoritative record of development after the
  fork** (the git history was squashed into a single initial commit per era),
  so keep it complete — a missing entry is an unrecoverable gap.

## Release-notes checklist (per release)

- [ ] `CHANGELOG.md` entry written and committed with the `VERSION` bump
- [ ] `VERSION` bumped via `scripts/bump-version.sh` (matching the cadence)
- [ ] Push to `main` → `bump-and-tag` created `v<VERSION>` (check GitHub)
- [ ] Gitea Release workflow built + pushed the image (check both registries)
- [ ] `port-status.md` updated: gaps fixed by this release moved to Closed
- [ ] Any `.base-image-digest` re-point since the last release is mentioned
      in the changelog entry
