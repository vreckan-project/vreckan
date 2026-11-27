# Paperclip control plane (source removed from this repo)

The paperclip01 control plane is the Paperclip platform
(v2026.1117.0 lineage). Since 2026-11-23 (ANT-1389) its source no
longer lives in this repo, and per CEO decision it was not
relocated to a new repo either — it is gone from our repos.

The running control plane is the image
`ghcr.io/paperclipai/paperclip:2026.1023.0` (the stock vendor
release, no dist-bake and no fork), rolled back to by the board's
direct action on 2026-11-24 1:46 PM MST (the go/no-go issue
ANT-1422 was cancelled as superseded). Its predecessor —
`ghcr.io/paperclipai/paperclip:2026.1117.0-wikidist`, deployed
2026-11-22 7:44 PM MST (the ANT-1322 cutover to the standard vendor
release + the thin wiki-plugin dist-bake overlay) — is retired from
service, and the prebuilt fork-overlay image
`ghcr.io/paperclipai/paperclip:ant-1311-main-ec8862b4c` (built from
this repo's main at `ec8862b4c` via the overlay
`Dockerfile.ant-1275-gate`, `FROM
ghcr.io/paperclipai/paperclip:2026.1117.0` + the files that diverge
from the vendor release) is retained alongside it as the rollback
backstop; the pre-swap DB backup is retained on the host with a
do-not-prune marker. Rollback-artifact disposition is pending the
Mon 11-30 weekly team review.

If a rebuild is ever needed, the pre-removal source is still
recoverable from this repo's git history: the merge commit
`b8bb44ed5` (tag `base-v2026.1117.0-gate`) and its post-merge
descendants carry the full tree, e.g.
`git show b8bb44ed5:Dockerfile.ant-1275-gate`; layer those files
over the vendor `2026.1117.0` release as the overlay build did.
The lineage branches (`history/base-v2026.1117.0`,
`history/pre-bump-scheduling-gate`) and the
`base-v2026.1117.0-gate` tag were deleted from this repo (GitHub +
Gitea mirror) when the removal merged.

For the vreckan *app* image, registries, and CI/CD flow see
[ops.md](./ops.md); for the release/changelog process see
[release.md](./release.md).
