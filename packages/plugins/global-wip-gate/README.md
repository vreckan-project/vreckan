# @paperclipai/global-wip-gate

At-gate run-ranking policy plugin for the Paperclip host's company-wide
in-flight run cap (the `schedulingGate` hook delivered with the host
scheduling gate).

It answers the host's once-per-claim-pass consultation with **defer** when a
scarce slot would land with a lower-ranked agent. It only ever *decreases*
slots or defers — it can never break the host cap, and if the worker dies the
host fails open to first-come ordering with nothing else changing.

## Ranking (the scoring brain)

For every queued run in the company, the plugin computes a comparable rank:

1. **Routines last** — runs whose issue is a routine execution
   (`issues.origin_kind = 'routine_execution'`, including plugin-operation
   routines) sort into a last tier. A routine override of `"first"` (keyed by
   routine id or issue id) lifts that routine into the normal tier.
2. **Blocking impact first** — how many company issues the run's issue
   *transitively* blocks (`issue_relations` type `blocks`), multiplied by the
   agent's configured weight (default 1).
3. **Issue priority** — critical → high → medium → low; runs with no issue
   rank below low.
4. **FIFO** — oldest `created_at` wins within a tier.

At the gate, the consulted agent's top queued run is compared against the
company-wide ranking. If strictly higher-ranked runs sit with *claimable*
agents (invokable status, healthy reporting chain, per-agent headroom > 0)
**and** the remaining host slots are scarce (`hostSlotsRemaining <=` the
superior count), the claim is deferred so the slot lands with the right
agent. With more slots than superiors everyone fits, so nothing is deferred.

**Aging fail-open:** a run deferred continuously for longer than
`ageOutMinutes` (default 5) stops being deferred — ordering reverts to FIFO
for it, the cap is untouched. Deferral bookkeeping resets when a run stops
being deferred, so the clock restarts on each deferral burst.

## Cache and fail-open behavior

- The ranking is cached in memory per company and refreshed on
  `issue.created` / `issue.updated` / `issue.relations.updated` /
  `agent.updated` / `agent.status_changed` / `agent.run.*` events, plus a
  one-minute `reconcile` job as a safety net.
- The consult hot path never queries the database: it answers from the
  cache. A cold or stale (older than 5 min) snapshot answers *no opinion*
  while a bounded background refresh runs — the host count always stands.
- Durable state (config, last snapshot, deferral bookkeeping) lives in
  company-scoped `plugin_state`, so ranking survives worker restarts: the
  first post-restart consult hydrates in a bounded 1.5 s slice and decides
  from the persisted snapshot when it is fresh.

## Dashboard (board API routes)

Mounted under `/api/plugins/<pluginId>/api/` (query param `companyId`):

| Method | Route | Auth | Purpose |
|--------|-------|------|---------|
| GET | `/status` | board-or-agent | enabled switch, live ranking, per-agent queued/running/headroom, deferrals with reasons |
| GET | `/config` | board-or-agent | current weights, routine overrides, age-out, sticky paused agents |
| PATCH | `/config` | board | update any of the above |
| POST | `/agent-override` | board | `sticky-pause` / `resume` an agent — the fallback lever for the comment/monitor-wake edge (wakes on a deferred agent still start outside the queue) |

`PATCH /config` body fields (all optional): `enabled` (bool),
`ageOutMinutes` (1–1440), `weights` (`{ agentId: number }`, 0 < w ≤ 100),
`routineOverrides` (`{ routineOrIssueId: "first" | "last" }`),
`stickyPausedAgents` (`string[]`).

`POST /agent-override` body: `{ agentId, action }` with `action` =
`"sticky-pause"` or `"resume"`.

## Install (local path — no npm publish)

```sh
pnpm --filter @paperclipai/global-wip-gate build
paperclipai plugin install /path/to/vreckan/packages/plugins/global-wip-gate
```

On the deployed instance the source tree is the repo checkout, so the path
is the installed plugin package inside the release tree (see the issue
runbook for the exact path on the server).

## Tests

```sh
pnpm --filter @paperclipai/global-wip-gate test        # unit (SDK harness)
pnpm --filter @paperclipai/global-wip-gate typecheck
```

End-to-end claim-order coverage lives in the server package
(`server/src/__tests__/global-wip-gate.integration.test.ts`): real host
claim path against embedded Postgres with the real plugin worker process.
