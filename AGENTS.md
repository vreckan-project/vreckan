# AGENTS.md

Working conventions for the agents (and humans) contributing to Vreckan.

## GitHub @-mention rule

On public GitHub (PRs, issues, reviews, commit messages), do **not** use
@-mentions to address or coordinate with a teammate agent — that pings their
public GitHub account and surfaces internal team chatter. Refer to teammates
by **plain name** (e.g. "Will", "Bobby") and route the actual coordination
through the Paperclip issue thread (the ANT-xxx issue). Reserve @-mentions
for when you genuinely need to ping a specific GitHub user (e.g. explicitly
requesting a code review from a maintainer).

## Gitea mirror push rule

Never push directly to the Gitea `vreckan/vreckan` mirror — feature branches
go to GitHub only, and `gitea-mirror-trigger.yml` is the sole Gitea writer
(`pr/<N>` branches; `bump-and-tag` for main/tags). See `docs/ops.md` §3.3.

## Wake-path (parking) rule (ANT-635)

Any run that parks an engineer issue must leave a **live wake path** before
it exits: a non-null `monitorNextCheckAt` (set via `executionPolicy.monitor`
on the issue PATCH) **or** a first-class blocker (`blockedByIssueIds`, or an
unblock descriptor owned by the run with a concrete action); a pending
board/approval interaction on the issue also counts as a live path. Parking
to `in_progress`/`in_review` with none of these leaves the issue with no
wake path at all — the board sees it "showing broken" and nothing re-wakes
the assignee. **Verify the path from the stored issue state after parking**:
the PATCH response must echo a non-null `monitorNextCheckAt` on an
agent-assigned, user-unassigned `in_progress`/`in_review` issue (the monitor
scheduler only fires for that combination) — "I set it" is not verification.
A run that cannot satisfy this must say so explicitly in its exit comment
("parked with no wake path — needs a side wake").

**Standing sweep (null-monitor stall check).** Owner: Boberta (PM). Cadence:
daily, standing section of the Daily standup routine (Mon–Fri 05:00 MST).
Any `in_progress`/`in_review` issue with `monitorNextCheckAt: null` and no
first-class blocker is a stall — except when it has an active run or a
pending interaction/approval constituting a live path. Stalls get (a) a
specific comment on the stalled issue asking the assignee to state and
restore the wake path, (b) a line each in the standup summary's "Stall
sweep" section, and (c) escalation to Bob (CEO) when the stall has been
stale for 2+ hours (heuristic: `lastActivityAt` older than 2 h at sweep
time).

## Local development environment (vreckan)

For local vreckan-rust work (build / test / run / debug), use the prebuilt
build/work image `repo.vreckanproject.com/vreckan/vreckan-base-build:latest`
(Rust 1.99 toolchain, `-sys` build deps, sccache, warm dependency cache —
see the `vreckan-workflow` repo, `images/vreckan-base-build/README.md`)
instead of assembling a toolchain on the host or inside the workspace.

**Bind-mount form (ANT-343).** The docker daemon resolves `-v` source paths
in the **host** namespace. In the agent-container layout the live Paperclip
data tree appears inside the container as `/paperclip/…`, but on the host it
lives at `/srv/paperclip/data/…`. Historically the host also carried a
stale, root-owned `/paperclip` directory (created 2026-10-15 by a
`docker run -v /paperclip/…` whose source the daemon auto-created), so
binding the in-container path `/paperclip/…` landed in that stale copy —
symptom: the build container sees a workspace view **without `Cargo.toml`**.
That misdiagnosis cost the 2026-10-19 ANT-328 core build run discovery time.

Use the daemon-side (host) form for any dev/build container bind: take the
path as seen inside the container and prefix `/paperclip` with
`/srv/paperclip/data`, e.g.

```
docker run --rm -it -v /srv/paperclip/data/workspaces/vreckan-rust:/app \
  repo.vreckanproject.com/vreckan/vreckan-base-build bash -l
```

Then work at `/app` — `cargo build --release`, `cargo test`, and `cargo run`
work as-is. Since ANT-343 the host also carries a
`/paperclip -> /srv/paperclip/data` symlink, so the bare `$PWD` form works
again too; the prefixed form is always safe. If a bind suddenly shows a
tree without `Cargo.toml`, suspect the mount form before the checkout.
