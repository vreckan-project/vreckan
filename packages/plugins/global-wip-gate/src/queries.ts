import {
  maxConcurrentRunsFor,
  type AgentInfo,
  type CompanySnapshot,
  type IssueInfo,
  type QueuedRunInfo,
} from "./types.js";

/**
 * Raw-SQL reads over the whitelisted core tables. Every table reference is
 * fully qualified with the `public` schema, as the plugin database validator
 * requires. Params are positional ($1, $2, ...).
 */

// hr.issue_id is uuid but the context-snapshot fallback is text: COALESCE
// would resolve to text and `i.id = <text>` has no operator, so the
// fallback is cast to uuid (NULLIF keeps an empty snapshot value a clean
// miss instead of a cast error).
const RUN_ISSUE_EXPR = `COALESCE(hr.issue_id, NULLIF(hr.context_snapshot ->> 'issueId', '')::uuid)`;

export const QUEUED_RUNS_SQL = `
SELECT
  hr.id AS run_id,
  hr.agent_id AS agent_id,
  ${RUN_ISSUE_EXPR} AS issue_id,
  hr.invocation_source AS invocation_source,
  hr.trigger_detail AS trigger_detail,
  hr.created_at AS created_at,
  i.origin_kind AS origin_kind,
  i.origin_id AS origin_id
FROM public.heartbeat_runs hr
LEFT JOIN public.issues i
  ON i.id = ${RUN_ISSUE_EXPR}
 AND i.company_id = hr.company_id
WHERE hr.company_id = $1
  AND hr.status = 'queued'
ORDER BY hr.created_at ASC, hr.id ASC
`;

export const AGENTS_SQL = `
SELECT
  a.id AS agent_id,
  a.name AS name,
  a.status AS status,
  a.reports_to AS reports_to,
  a.runtime_config AS runtime_config,
  count(r.id) FILTER (WHERE r.status = 'running') AS running
FROM public.agents a
LEFT JOIN public.heartbeat_runs r
  ON r.agent_id = a.id AND r.company_id = a.company_id
WHERE a.company_id = $1
GROUP BY a.id, a.name, a.status, a.reports_to, a.runtime_config
ORDER BY a.name ASC, a.id ASC
`;

export const ISSUES_SQL = `
SELECT
  i.id AS issue_id,
  i.priority AS priority,
  i.origin_kind AS origin_kind,
  i.origin_id AS origin_id,
  (
    SELECT count(*) FROM public.issue_relations er
    WHERE er.company_id = i.company_id AND er.issue_id = i.id AND er.type = 'blocks'
  ) AS direct_block_count
FROM public.issues i
WHERE i.company_id = $1
  AND i.status NOT IN ('done', 'cancelled')
ORDER BY i.id ASC
`;

/**
 * All blocking edges for the company, used to compute transitive blocking
 * impact (how many issues each issue transitively blocks).
 */
export const RELATIONS_SQL = `
SELECT
  ir.issue_id AS blocker_id,
  ir.related_issue_id AS blocked_id
FROM public.issue_relations ir
WHERE ir.company_id = $1
  AND ir.type = 'blocks'
ORDER BY ir.issue_id ASC, ir.related_issue_id ASC
`;

export interface QueuedRunRow {
  run_id: string;
  agent_id: string;
  issue_id: string | null;
  invocation_source: string | null;
  trigger_detail: string | null;
  created_at: string;
  origin_kind: string | null;
  origin_id: string | null;
}

export interface AgentRow {
  agent_id: string;
  name: string;
  status: string;
  reports_to: string | null;
  runtime_config: Record<string, unknown> | null;
  running: number;
}

export interface IssueRow {
  issue_id: string;
  priority: string;
  origin_kind: string;
  origin_id: string | null;
  direct_block_count: number;
}

export interface RelationRow {
  blocker_id: string;
  blocked_id: string;
}

/**
 * Compute transitive blocked counts for every issue from the blocking-edge
 * list: impact(X) = |{Y : X transitively blocks Y}|. BFS per source with a
 * visited set, so cycles and diamond graphs count each reachable issue once.
 */
export function transitiveBlockedCounts(
  issueIds: string[],
  edges: RelationRow[],
): Map<string, number> {
  const children = new Map<string, string[]>();
  for (const edge of edges) {
    const list = children.get(edge.blocker_id) ?? [];
    list.push(edge.blocked_id);
    children.set(edge.blocker_id, list);
  }
  const counts = new Map<string, number>();
  for (const issueId of issueIds) {
    const reachable = new Set<string>();
    const stack = [...(children.get(issueId) ?? [])];
    while (stack.length > 0) {
      const current = stack.pop()!;
      // A cycle back to the source is not the source blocking itself.
      if (current === issueId || reachable.has(current)) continue;
      reachable.add(current);
      const next = children.get(current);
      if (next) stack.push(...next);
    }
    counts.set(issueId, reachable.size);
  }
  return counts;
}

/**
 * Assemble the company snapshot from the raw rows. Pure function — the
 * worker feeds it query results and the tests feed it fixtures.
 */
// A refresh is all-or-nothing (Promise.all over the reads): when it fails the
// worker keeps the previous snapshot untouched, and when it succeeds the DB
// view of queued runs is authoritative. There is therefore no "partial
// refresh" to heal — merging a prior snapshot would only re-add runs that
// left the queue and keep deferring lower-ranked agents until age-out.
export function buildSnapshotFromRows(input: {
  companyId: string;
  nowMs: number;
  queuedRuns: QueuedRunRow[];
  agents: AgentRow[];
  issues: IssueRow[];
  relations: RelationRow[];
}): CompanySnapshot {
  const { companyId, nowMs, queuedRuns, agents, issues, relations } = input;

  const blockedCounts = transitiveBlockedCounts(
    issues.map((row) => row.issue_id),
    relations,
  );
  const issueMap: Record<string, IssueInfo> = {};
  for (const row of issues) {
    issueMap[row.issue_id] = {
      priority: row.priority,
      originKind: row.origin_kind ?? "manual",
      originId: row.origin_id,
      blockedCount: blockedCounts.get(row.issue_id) ?? Number(row.direct_block_count ?? 0),
    };
  }

  const agentMap: Record<string, AgentInfo> = {};
  for (const row of agents) {
    agentMap[row.agent_id] = {
      agentId: row.agent_id,
      name: row.name,
      status: row.status,
      reportsTo: row.reports_to,
      maxConcurrentRuns: maxConcurrentRunsFor(row.runtime_config, 2),
      running: Number(row.running ?? 0),
      queued: [],
    };
  }

  for (const row of queuedRuns) {
    const agent = agentMap[row.agent_id];
    if (!agent) continue;
    const routineOrigin =
      row.origin_kind === "routine_execution" ||
      (row.origin_kind?.startsWith("plugin:") ?? false);
    const run: QueuedRunInfo = {
      runId: row.run_id,
      agentId: row.agent_id,
      issueId: row.issue_id,
      routineOriginId: routineOrigin ? row.origin_id : null,
      routine: routineOrigin,
      createdAtMs: Date.parse(row.created_at),
      invocationSource: row.invocation_source,
      triggerDetail: row.trigger_detail,
    };
    agent.queued.push(run);
    // Keep the issue the run refers to in the snapshot even when it is done
    // (the rank still needs its priority/impact fields).
    if (row.issue_id && !issueMap[row.issue_id]) {
      issueMap[row.issue_id] = {
        priority: "medium",
        originKind: row.origin_kind ?? "manual",
        originId: row.origin_id,
        blockedCount: blockedCounts.get(row.issue_id) ?? 0,
      };
    }
  }

  const snapshot: CompanySnapshot = {
    companyId,
    refreshedAtMs: nowMs,
    agents: agentMap,
    issues: issueMap,
  };

  for (const agent of Object.values(agentMap)) {
    agent.queued.sort((a, b) => a.createdAtMs - b.createdAtMs || a.runId.localeCompare(b.runId));
  }
  return snapshot;
}
