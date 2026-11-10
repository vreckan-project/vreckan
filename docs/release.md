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

The two coordinate per release: for explicit releases the changelog entry is
**authored before the tag is cut** (it ships in the same commit as the
`VERSION` bump), so the release notes and the image tag always agree.
Ordinary merges need no changelog entry — they get auto-composed release
notes (below).

## The flow

**Per-push versioning (ANT-1050): every push to `main` is a release.** The
`bump-and-tag` job (GitHub Actions, runs on every push to `main`, serialized
by a concurrency group) decides which path this push takes from the current
`VERSION`:

- **Tag path** — tag `v<VERSION>` does not exist yet (this push changed
  `VERSION`: an explicit release PR, or the bot's own bump commit from the
  bump path). The job mints a short-lived GitHub App installation token,
  creates and pushes the annotated tag `v<VERSION>`, mirrors the tag (plus
  the commit as `main`) to the Gitea mirror, dispatches the release
  workflow, and creates the GitHub Release entry.
- **Bump path** — tag `v<VERSION>` already exists (this push carries
  unreleased changes on an already-released version — the common case). The
  job runs `scripts/bump-version.sh patch`, commits the
  `VERSION`/`Cargo.toml`/`Cargo.lock` rewrite as vreckan-ci[bot], and lands
  the commit on `main` in two stages: first it pushes it to a staging
  branch `release/bump-<new>` (whose push mirrors the branch and dispatches
  the Gitea build, which posts the `VRECKAN: Smoke/Test/Scan (Rust)` commit
  statuses that the `main` branch rules require on every commit pushed to
  `main` — a fresh bot commit has none of them yet, so a direct push is
  rejected), then, once those checks are green, it pushes the commit to
  `main`. That push re-runs the job and takes the tag path, so the release
  is cut automatically. **The loop terminates**: at most one bot commit per
  push, because the bump commit's own run sees a version whose tag does not
  exist. If `main` moves under the run (back-to-back merges), the bump
  re-derives from the new tip and retries once (re-staging the fresh
  commit), so each merge gets its own sequential number (0.11.10, 0.11.11,
  …) instead of racing for the same tag.

### Explicit releases (minor / major)

Minor and major bumps remain an explicit release-PR decision; per-push patch
numbering continues from them (0.12.0 → 0.12.1 → …).

1. **Prepare on `main`.** The Documentation owner writes the `CHANGELOG.md`
   entry for the release (convention below) and bumps `VERSION` in the same
   branch:
   ```bash
   ./scripts/bump-version.sh minor   # or: major
   ```
   `scripts/bump-version.sh` is the single source of version math, shared by
   the GitHub and Gitea-side workflows. Beyond `VERSION` it re-points the
   root `Cargo.toml` `[workspace.package] version` and the matching
   `Cargo.lock` entries of every workspace crate in the same run, and
   verifies all the files read back the new version. Commit the changelog
   entry and every file change the script makes together, and push to
   `main` → the tag path cuts the release.
2. **Release notes** for such a PR are the `CHANGELOG.md` section it adds —
   the tag path detects the added section in the push range and uses it.

### Ordinary merges

Ordinary PRs (features, fixes, docs, dependency bumps) merge to `main` with
no `VERSION` change and no changelog entry. `bump-and-tag` takes the bump
path: the bot mints the next patch number, and the GitHub Release notes are
**auto-composed from the push range's commit subjects** (merge-commit
subjects are excluded — they add nothing the branch commits don't say). A
push range that adds a `CHANGELOG.md` section gets that section as notes
instead (explicit releases above).

### Release publication and recovery

1. The tag path dispatches the **Gitea Release workflow**
   (`release-rust.yml` in the `vreckan/vreckan-workflow` repo) with the
   `tag` input (Gitea Actions has no cross-repo event routing, so a tag
   push to the mirror cannot fire a workflow in the workflow repo — the
   tag-push trigger has been dormant since the workflow left the mirror's
   tree, ANT-1050). The workflow clones the tag from the mirror, builds the
   image on the self-hosted runner, and pushes it to **GHCR**
   (`ghcr.io/vreckan-project/vreckan`) and the **Gitea registry**, with the
   versioned tag and `latest`. A "skip if already published" guard inside
   the workflow makes duplicate dispatches a no-op (it skips the rebuild
   when the tag's image is already published at the tag's commit), so
   recovery dispatches are safe to repeat.
2. A dispatch failure is non-fatal in `bump-and-tag` (the tag and the
   GitHub Release must land even if the dispatch hiccups — the v0.10.1
   orphaned-Release precedent, ANT-703/ANT-714). Recover with a manual
   `workflow_dispatch` of `release-rust.yml` (required `tag` input); the
   daily freshness run's "tag without image" early warning flags a tag that
   never got an image (ANT-1050).
3. **Post-release check.** Verify the tag exists on GitHub, the image tags
   are present in both registries, and the compose file's
   `ghcr.io/...:latest` reference resolves. Because every merge is a
   release, this is effectively a per-push deployment check — the
   deployment tracks `:latest`, so every merge (including docs-only and
   dependency PRs) reaches vreckan.antitux.net within minutes of landing.

### Base-image bumps

When the nightly base-bump routine re-points `.base-image-digest` (build
base digest drifted), the PR also re-points the `Dockerfile` build-stage
digest pin (ANT-277) and carries a `CHANGELOG.md` entry noting the base
change ("build base advanced to digest …") plus the `VERSION` patch bump.
Under per-push versioning the merge cuts its own new tag and release —
**a published tag is never re-pointed** (the v0.11.9 mid-day re-cut,
ANT-1050, is what motivated the rule).

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

## Release-notes checklist

**Explicit releases (minor / major):**

- [ ] `CHANGELOG.md` entry written and committed with the `VERSION` bump
- [ ] `VERSION` bumped via `scripts/bump-version.sh` (minor | major)
- [ ] Push to `main` → `bump-and-tag` took the tag path and created
      `v<VERSION>` (check GitHub)
- [ ] Gitea Release workflow built + pushed the image (check both registries)
- [ ] `port-status.md` updated: gaps fixed by this release moved to Closed
- [ ] Any `.base-image-digest` re-point since the last release is mentioned
      in the changelog entry

**Per-push patch releases (every ordinary merge):**

- [ ] GitHub Release entry exists with the auto-composed notes
- [ ] The `release-rust` run is green and both registries carry the tag at
      the tag's commit (`vreckan.source-sha` label)
- [ ] `:latest` tracks the new tag
