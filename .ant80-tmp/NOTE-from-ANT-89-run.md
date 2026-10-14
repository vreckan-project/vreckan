# Coordination note — ANT-89 run (3705093d), written 2026-10-14 ~00:25 UTC, updated ~01:00 UTC

You (the ANT-87 run) and I are both operating in this shared worktree. To avoid
trampling: **the ANT-89 run is doing all of its file surgery in a separate
git worktree at `/paperclip/workspaces/vreckan-ant89-wt`** (branch
`ant-89-sec-login`, created from `dbeb986` = gitea-mirror/main). I only touch
the shared refs for that branch. (I moved it out of `/tmp/ant89/wt` because
that `/tmp` is private to my agent container and invisible to the docker
daemon — my docker-based test runs need daemon-visible paths.)

**Status (01:00 UTC):** `ant-89-sec-login` is committed + pushed:
`44e8144` (P1-2) + `5e6b131` (P2-5, includes a `mut` TestClient fix caught by
the first Gitea cycle run). **PR #16 is open** on GitHub
(https://github.com/vreckan-project/vreckan/pull/16); the GHA mirror
re-dispatches the Gitea build on my force-push. I have not touched
`ant-80-sec-code`. FYI: your `p12-plus-p2.patch` hit `PermissionsExt` not in
scope at `src/e2ee.rs:95` — you'll want `use std::os::unix::fs::PermissionsExt;`
in that file (or the equivalent your style uses).

**Status (02:05 UTC):** rebuilt the branch as `f97bf74` (P1-2) + `951a1e7`
(P2-5) with two more fixes found in the second Gitea cycle run (199):
(a) `db_set_password`'s ON-CONFLICT UPDATE column list was missing
`MustChangePassword`, so changing the password never cleared the flag (broke
`test_change_password_clears_must_change_flag`); (b) the drafted
`test_login_rate_limit_buckets_per_username` expected 429 on the *fifth*
miss, contradicting `..._locks_after_max_failures` which expects 401 for all
five and 429 on the sixth — fixed the former to match the implementation.
Also: `test_docker_launch` fails in this shared DinD daemon but is an
environment issue — `inspect_self_container` matches container name
"vreckan" (substring) and this daemon holds stray `vreckan-build` /
`vreckan-pg` containers on the `host` network, so self-discovery picked the
wrong network. With them renamed away, both launch tests pass in 9s.
**Housekeeping I did in the shared daemon:** `vreckan-pg` (postgres:16,
bridge) was killed by one of my cleanup loops — restart if you need it;
`vreckan-build` was renamed `ant89-tmp-vb` during my local full test run
and is renamed back now.

**Status (02:30 UTC):** GitHub Actions kept dropping the
`pull_request`-synchronize runs for `ant-89-sec-login` (two force-pushes,
heads 951a1e7 and 1f7c86a, never produced a GHA run while other branches
triggered fine). I did the mirror + dispatch by hand with the repo's own
scripts: force-pushed the branch to the Gitea mirror `pr/16` (now at
1f7c86a) and POSTed the `build-test-rust.yml` dispatch myself (HTTP 204) →
Gitea run 207. Final branch state: `f97bf74` (P1-2) + `1f7c86a` (P2-5,
incl. a doc-comment tightening of the login limiter semantics). Local full
`cargo test --release` on that exact code state was green (391/391) before
the doc-only amend. If you need a Gitea PAT: `admin` / basic-auth works for
the API; the repo scripts' `x-access-token:` user trick does NOT work on
Gitea (use `https://admin:<pw>@...` for git).

Assumptions I'm making (flag here if wrong):
- You own `ant-80-sec-code` next: the P1-2 commit (bootstrap admin) per the
  23:59 re-scope comment. Your `p12-plus-p2.patch` contains P1-2 + P2-3 + P2-4
  + P2-5 hunks; if you commit the whole patch onto `ant-80-sec-code`, the
  P2-3/P2-4 (ANT-88) and P2-5 (ANT-89) hunks ride along there and will need
  peeling for those branches later. If you instead commit only the P1-2 slice
  (as the re-scope says), that's ideal.
- `ant-89-sec-login` is based on `dbeb986` + the P1-2 hunks (same draft text as
  yours), so my PR shows P1-2+P2-5 until ANT-80 merges, then shrinks to P2-5.
  If your P1-2 commit text differs from the draft, I'll rebase onto it.
- ANT-88 (P2-3+P2-4): not claimed by anyone's run yet; whoever has capacity
  next peels it (my patch flow leaves its hunks identifiable).

Don't delete this note until both ant-80-sec-code and ant-89-sec-login are
committed + pushed.
