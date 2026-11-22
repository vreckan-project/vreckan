# Vreckan (Rust) — Paperclip control plane: tree, image build, tag rule, rollback

The paperclip01 control plane is the Paperclip platform server built from
this repo. Since the v2026.1117.0 base lineage landed on `main`
(2026-11-22, ANT-1311 / ANT-1312; merge commit `b8bb44ed5`) the repo tree
is the union of the vreckan Rust app and the full platform source tree, and
the control-plane image is built from this repo. This note is the operator
reference for that build. For the vreckan *app* image, registries, and the
CI/CD flow see [ops.md](./ops.md); for the release/changelog process see
[release.md](./release.md); for what shipped, see the
[2026-11-22 CHANGELOG entry](../CHANGELOG.md).

## 1. Where the platform tree lives in the repo

The unrelated-histories merge (parents `1490a2987` + `3f76d306`) resolved
the 7 root-file conflicts by keeping the vreckan files at the root and
relocating the platform's:

| Path | Contents |
|---|---|
| `server/` | The platform's Express control-plane API (`server/src`, `server/package.json` = `@paperclipai/server`), **coexisting with** the vreckan web client (`server/static`) and `server/template_schema.yml`. The root `Dockerfile` (vreckan-app image) builds the client side; `Dockerfile.ant-1275-gate` (§2) builds the API side. |
| `ui/` | The platform's React + Vite board UI. |
| `packages/`, `cli/` | Platform packages (adapters, `db` (Drizzle schema + migrations), `shared`, plugins incl. `plugin-llm-wiki`, …) and the CLI. |
| `doc/` | Platform development docs — `DEVELOPING.md`, `DATABASE.md`, the plugins spec, … |
| `rules/`, `skills/`, `announcements/` | Platform scoped rules (`rules/*.mdc`), company skills, announcement templates. |
| `releases/` | The platform's per-release notes (e.g. `releases/v2026.1117.0.md`). |
| `docker/Dockerfile.platform-base` | The platform's node-based image Dockerfile, **relocated from the root** by the merge. Build from the repo root: `docker build -f docker/Dockerfile.platform-base .` |
| `Dockerfile.ant-1275-gate` | The control-plane **overlay** Dockerfile — §2. |
| `docs/platform/README.md` | The platform's own README, preserved verbatim (linked from the root README's "Platform base tree" section). |
| `docs/platform/AGENTS.md` | The platform's working conventions, preserved verbatim. |
| `docs/platform/LICENSE.md` | The platform's MIT license (the repo root keeps Vreckan's MPL-2.0). |
| `.github/workflows/` | Both trees' workflows coexist. Five platform-only workflows — `commitperclip-review`, `refresh-lockfile`, `runner-chaos-evals`, the `pr.yml` job, and the `docker-runner-check` PR leg — are gated on `github.repository == 'paperclipai/paperclip'` so they stay inert in this repo. |

Working conventions for platform code (read `doc/DEVELOPING.md` /
`doc/DATABASE.md` and the scoped `rules/*.mdc` before touching it):
[docs/platform/AGENTS.md](./platform/AGENTS.md).

## 2. How the control-plane image is built (the overlay pattern)

`Dockerfile.ant-1275-gate` is an **overlay** build, not a from-scratch
build — it starts from the vendor release image and layers only the files
that diverge from it:

```dockerfile
FROM ghcr.io/paperclipai/paperclip:2026.1117.0
ARG PAPERCLIP_BUILD_COMMIT=<sha>
COPY <the diverging platform files> <same paths>   # scheduling gate: migration 0452, schema, sdk, heartbeat, …
RUN PAPERCLIP_BUILD_COMMIT=$PAPERCLIP_BUILD_COMMIT pnpm --filter @paperclipai/plugin-sdk build
RUN PAPERCLIP_BUILD_COMMIT=$PAPERCLIP_BUILD_COMMIT pnpm --filter @paperclipai/plugin-llm-wiki build
RUN PAPERCLIP_BUILD_COMMIT=$PAPERCLIP_BUILD_COMMIT sh -c "cd server && tsc && … && node scripts/write-build-stamp.mjs"
```

Notes:

- It is built **locally on paperclip01** (`docker build
  -f Dockerfile.ant-1275-gate -t <tag> .` from the repo checkout); the
  image tag is local-only unless it is pushed.
- The `COPY` set is the scheduling-gate delta (company scheduling
  migration `0452_company_scheduling`, the Drizzle schema, the plugin SDK,
  the heartbeat `scheduling-gate` service, …). When the divergence from the
  vendor base grows, the COPY list must grow with it — a file that changed
  on `main` but is not COPYed stays at the vendor version in the image.
- The `plugin-llm-wiki` dist build is mandatory (ANT-1279): no upstream
  release image ships `packages/plugins/plugin-llm-wiki/dist`, and the
  v2026.1117.0 plugin loader requires `dist/manifest.js` on disk at
  activation — the final `RUN test …` guard fails the build if the dist is
  missing.
- `write-build-stamp.mjs` writes `server/dist/build-info.json` carrying the
  `PAPERCLIP_BUILD_COMMIT` value — this is what surfaces as the image's
  reported build revision.

## 3. Tag / revision metadata rule

**The image tag must equal the commit the image was actually built from,
and the reported build revision must say the same thing.**

The reason this is a rule, not a nicety, is the ANT-1275 anomaly (full
explanation in the [2026-11-22 CHANGELOG
entry](../CHANGELOG.md)): the image paperclip01 ran before the landing was
tagged `ant-1275-gate-c261ab6` while its reported build revision was
`c0f132898`. Two different SHAs, two different sources:

- The **tag** carried the Dockerfile's `PAPERCLIP_BUILD_COMMIT` ARG — the
  **pre-bump** scheduling-gate commit `c261ab6` (ANT-906), from the gate
  branch before the 2026-11-20 rebase.
- The **revision label** (`org.opencontainers.image.revision`) is inherited
  from the vendor base image on the `FROM` line
  (`ghcr.io/paperclipai/paperclip:2026.1117.0`, which upstream built at
  `c0f132898`).

Not a broken build — but the tag named a commit that is *not* what the
image was built from, which defeats "which source produced this running
image?" during incident response. Consequences for every future control-
plane build:

1. Pass the **actual build commit** as `PAPERCLIP_BUILD_COMMIT`
   (`--build-arg PAPERCLIP_BUILD_COMMIT=<sha of the checkout you build
   from>`) — do not rely on the ARG default in the Dockerfile (the
   current default is the historical pre-bump SHA `c261ab6`).
2. Tag the image with that same SHA (e.g. `ant-<issue>-<short-sha>`).
3. Verify after the build: the tag, `build-info.json`, and the image's
   revision metadata must all carry the same SHA.
4. The 2026-11-22 redeploy (ANT-1314) was the first build under this rule —
   it shipped as `ant-1311-main-ec8862b4c` with the tag and revision label
   both carrying the actual merged-`main` SHA (`ec8862b4c`); all five
   metadata sources (tag, revision, version, source, and the running
   container's reported revision) agreed after deploy.

## 4. Rollback artifact policy

- The **previous** control-plane image is retained (local + registry) as
  the rollback artifact for the next deployment. It is **never deleted
  before the replacement deployment is verified healthy** — for the
  2026-11-22 redeploy that verification is the independent post-deploy
  QA pass + release sign-off (ANT-1315).
- Rollback = re-tag/repoint the retained old image and restart the
  control-plane container from it; the database migrations it shipped stay
  applied, so the old image must still run against the new schema (the
  scheduling-gate migrations are additive; verify before relying on a
  rollback).
- Whether a retired image stays in ghcr as a **permanent** rollback
  artifact (vs. local-only) is a board decision taken after sign-off —
  `ant-1275-gate-c261ab6` is that pending case.
- Related: the `history/` branches are the durable *source* record and are
  not deletion targets — see §5.

## 5. Lineage refs (permanent record)

| Ref | Points at | Meaning |
|---|---|---|
| branch `history/base-v2026.1117.0`, tag `base-v2026.1117.0-gate` | `3f76d306` | The v2026.1117.0 base lineage as merged into `main` (vendor base `c0f132898` + rebased gate work: `78e1260` ANT-902, `254af40` ANT-906, `cb8b6c3` ANT-1275, `d83cfd4` ANT-1279, `b9527a2`/`d8b4a74` ANT-1300). |
| branch `history/pre-bump-scheduling-gate` | `2bc30c13a` (ANT-904) | The **pre-rebase** gate chain: `2604d86` (ANT-902) → `c261ab6` (ANT-906, the SHA the old image tag named) → `2bc30c13a` (ANT-904). Preserved because the pre-bump branches were deleted and `c261ab6` would otherwise be an orphan. |

Both are `history/`-prefix branches — the sweep exemption (precedent
`history/pre-squash`) keeps them out of branch cleanup. Do not delete or
repoint them; if the lineage is re-synced to a newer upstream release, add
the new base branch alongside rather than moving these.
