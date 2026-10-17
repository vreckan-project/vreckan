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
