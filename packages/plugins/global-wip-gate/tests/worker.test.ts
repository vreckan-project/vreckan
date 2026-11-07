import { describe, expect, it } from "vitest";
import { createTestHarness } from "@paperclipai/plugin-sdk/testing";
import type { Agent, PaperclipPluginManifestV1, PluginContext } from "@paperclipai/plugin-sdk";
import manifest from "../src/manifest.js";
import plugin, { __resetForTests } from "../src/worker.js";
import { AGENTS_SQL, ISSUES_SQL, QUEUED_RUNS_SQL, RELATIONS_SQL } from "../src/queries.js";
import type { AgentRow, IssueRow, QueuedRunRow, RelationRow } from "../src/queries.js";
import type { CompanySnapshot, GateConfig } from "../src/types.js";

const COMPANY = "11111111-1111-4111-8111-111111111111";
const SELF = "22222222-2222-4222-8222-222222222222";
const OTHER = "33333333-3333-4333-8333-333333333333";
const SELF_RUN = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER_RUN = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const BASE = Date.parse("2026-11-07T09:00:00.000Z");

function makeAgent(id: string, overrides: Partial<Agent> = {}): Agent {
  return {
    id,
    companyId: COMPANY,
    name: `agent-${id.slice(0, 4)}`,
    urlKey: id,
    role: "general",
    title: null,
    icon: null,
    status: "idle",
    reportsTo: null,
    capabilities: null,
    adapterType: "process",
    adapterConfig: {},
    runtimeConfig: {},
    budgetMonthlyCents: 0,
    spentMonthlyCents: 0,
    pauseReason: null,
    pausedAt: null,
    permissions: { canCreateAgents: false },
    lastHeartbeatAt: null,
    metadata: null,
    createdAt: new Date(BASE),
    updatedAt: new Date(BASE),
    ...overrides,
  };
}

type QueryRouter = (sql: string, params?: unknown[]) => unknown[];

/** Replace the harness db with a SQL-routed stub and expose the row fixtures. */
function stubDb(ctx: PluginContext, route: QueryRouter) {
  ctx.db.query = (async (sql: string, params?: unknown[]) => route(sql, params)) as typeof ctx.db.query;
}

function fixtureRows(): {
  queuedRuns: QueuedRunRow[];
  agents: AgentRow[];
  issues: IssueRow[];
  relations: RelationRow[];
} {
  const iso = (ms: number) => new Date(ms).toISOString();
  return {
    queuedRuns: [
      {
        run_id: OTHER_RUN,
        agent_id: OTHER,
        issue_id: "issue-other",
        invocation_source: "on_demand",
        trigger_detail: "manual",
        created_at: iso(BASE - 1000),
        origin_kind: "manual",
        origin_id: null,
      },
      {
        run_id: SELF_RUN,
        agent_id: SELF,
        issue_id: "issue-self",
        invocation_source: "on_demand",
        trigger_detail: "manual",
        created_at: iso(BASE),
        origin_kind: "manual",
        origin_id: null,
      },
    ],
    agents: [
      { agent_id: SELF, name: "self", status: "idle", reports_to: null, runtime_config: null, running: 0 },
      { agent_id: OTHER, name: "other", status: "idle", reports_to: null, runtime_config: null, running: 0 },
    ],
    issues: [
      { issue_id: "issue-self", priority: "high", origin_kind: "manual", origin_id: null, direct_block_count: 0 },
      { issue_id: "issue-other", priority: "low", origin_kind: "manual", origin_id: null, direct_block_count: 3 },
    ],
    relations: [
      { blocker_id: "issue-other", blocked_id: "d1" },
      { blocker_id: "issue-other", blocked_id: "d2" },
      { blocker_id: "issue-other", blocked_id: "d3" },
    ],
  };
}

async function setupHarness(opts: {
  route?: QueryRouter;
  seedAgents?: Agent[];
  persisted?: { config?: unknown; snapshot?: CompanySnapshot | null; deferrals?: unknown };
}) {
  // The worker keeps module-level state; start every test from a cold worker.
  __resetForTests();
  const harness = createTestHarness({ manifest: manifest as PaperclipPluginManifestV1 });
  const rows = fixtureRows();
  const route: QueryRouter = opts.route ?? ((sql, params) => {
    const companyId = params?.[0];
    if (companyId !== COMPANY) return [];
    if (sql === QUEUED_RUNS_SQL) return rows.queuedRuns;
    if (sql === AGENTS_SQL) return rows.agents;
    if (sql === ISSUES_SQL) return rows.issues;
    if (sql === RELATIONS_SQL) return rows.relations;
    return [];
  });
  stubDb(harness.ctx, route);
  if (opts.seedAgents) harness.seed({ agents: opts.seedAgents });
  if (opts.persisted) {
    if (opts.persisted.config !== undefined) {
      await harness.ctx.state.set(
        { scopeKind: "company", scopeId: COMPANY, namespace: "wip-gate", stateKey: "config" },
        opts.persisted.config,
      );
    }
    if (opts.persisted.snapshot !== undefined) {
      await harness.ctx.state.set(
        { scopeKind: "company", scopeId: COMPANY, namespace: "wip-gate", stateKey: "snapshot" },
        opts.persisted.snapshot,
      );
    }
    if (opts.persisted.deferrals !== undefined) {
      await harness.ctx.state.set(
        { scopeKind: "company", scopeId: COMPANY, namespace: "wip-gate", stateKey: "deferrals" },
        opts.persisted.deferrals,
      );
    }
  }
  await plugin.definition.setup(harness.ctx);
  return { harness, rows };
}

async function waitFor(condition: () => boolean, timeoutMs = 2000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error("waitFor timed out");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

async function gateCall(
  ctx: PluginContext,
  overrides: {
    agentId?: string;
    runId?: string;
    issueId?: string | null;
    hostSlotsRemaining?: number;
    companyId?: string;
  } = {},
) {
  return plugin.definition.onSchedulingGate!({
    companyId: overrides.companyId ?? COMPANY,
    agentId: overrides.agentId ?? SELF,
    run: {
      runId: overrides.runId ?? SELF_RUN,
      agentId: overrides.agentId ?? SELF,
      companyId: overrides.companyId ?? COMPANY,
      status: "queued",
      issueId: overrides.issueId ?? "issue-self",
      invocationSource: "on_demand",
      triggerDetail: "manual",
      createdAt: new Date(BASE).toISOString(),
    },
    hostSlotsRemaining: overrides.hostSlotsRemaining ?? 1,
  });
}

function apiCall(ctx: PluginContext, input: {
  routeKey: string;
  method: string;
  body?: unknown;
  companyId?: string;
}) {
  return plugin.definition.onApiRequest!({
    routeKey: input.routeKey,
    method: input.method,
    path: "",
    params: {},
    query: { companyId: input.companyId ?? COMPANY },
    body: input.body ?? {},
    actor: { actorType: "user", actorId: "user-1", agentId: null, userId: "user-1", runId: null },
    companyId: input.companyId ?? COMPANY,
    headers: {},
  });
}

describe("global-wip-gate worker", () => {
  it("registers the reconcile job and event subscriptions in setup", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    // A consult registers the company with the worker; reconcile then
    // refreshes it (the safety net only keeps *known* companies warm).
    await gateCall(harness.ctx);
    await waitFor(() =>
      Boolean(
        harness.getState({
          scopeKind: "company",
          scopeId: COMPANY,
          namespace: "wip-gate",
          stateKey: "snapshot",
        }),
      ),
    );
    await expect(harness.runJob("reconcile")).resolves.toBeUndefined();
    // The reconcile sweep refreshed the known company.
    const snapshot = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "snapshot",
    }) as CompanySnapshot;
    expect(snapshot.companyId).toBe(COMPANY);
    expect(Object.keys(snapshot.agents)).toContain(SELF);
  });

  it("fails open on the first (cold) consult, then hydrates and refreshes in the background", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    const first = await gateCall(harness.ctx);
    expect(first).toEqual({});

    await waitFor(() => {
      const snapshot = harness.getState({
        scopeKind: "company",
        scopeId: COMPANY,
        namespace: "wip-gate",
        stateKey: "snapshot",
      }) as CompanySnapshot | null;
      return Boolean(snapshot);
    });

    const second = await gateCall(harness.ctx);
    // other's run blocks 3 issues; self's none → other ranks first and the
    // single remaining slot is scarce → self's claim is deferred.
    expect(second.defer).toBe(true);
    expect(second.reason).toMatch(/higher-ranked/);

    const deferrals = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "deferrals",
    }) as Record<string, { firstDeferredAtMs: number }>;
    expect(deferrals[SELF_RUN]).toBeDefined();
  });

  it("serves a persisted fresh snapshot after a worker restart without a DB round-trip on the consult", async () => {
    // Simulate a previous life: a persisted snapshot that is still fresh.
    const { harness } = await setupHarness({
      seedAgents: [makeAgent(SELF), makeAgent(OTHER)],
      persisted: {
        snapshot: {
          companyId: COMPANY,
          refreshedAtMs: Date.now() - 10_000,
          agents: {
            [SELF]: {
              agentId: SELF,
              name: "self",
              status: "idle",
              reportsTo: null,
              maxConcurrentRuns: 2,
              running: 0,
              queued: [
                {
                  runId: SELF_RUN,
                  agentId: SELF,
                  issueId: "issue-self",
                  routineOriginId: null,
                  routine: false,
                  createdAtMs: BASE,
                  invocationSource: "on_demand",
                  triggerDetail: "manual",
                },
              ],
            },
            [OTHER]: {
              agentId: OTHER,
              name: "other",
              status: "idle",
              reportsTo: null,
              maxConcurrentRuns: 2,
              running: 0,
              queued: [
                {
                  runId: OTHER_RUN,
                  agentId: OTHER,
                  issueId: "issue-other",
                  routineOriginId: null,
                  routine: false,
                  createdAtMs: BASE - 1000,
                  invocationSource: "on_demand",
                  triggerDetail: "manual",
                },
              ],
            },
          },
          issues: {
            "issue-self": { priority: "high", originKind: "manual", originId: null, blockedCount: 0 },
            "issue-other": { priority: "low", originKind: "manual", originId: null, blockedCount: 3 },
          },
        },
      },
      // A route that fails loudly if the consult path ever queries the DB.
      route: (sql) => {
        if (sql === QUEUED_RUNS_SQL || sql === AGENTS_SQL || sql === ISSUES_SQL || sql === RELATIONS_SQL) {
          throw new Error(`unexpected DB query on the hot path: ${sql}`);
        }
        return [];
      },
    });
    // First consult hydrates state (no DB snapshot reads), then decides.
    const first = await gateCall(harness.ctx);
    expect(first.defer).toBe(true);
  });

  it("keeps the host count standing when the snapshot is stale and the refresh keeps failing", async () => {
    const staleSnapshot: CompanySnapshot = {
      companyId: COMPANY,
      refreshedAtMs: Date.now() - 6 * 60_000,
      agents: {
        [SELF]: {
          agentId: SELF,
          name: "self",
          status: "idle",
          reportsTo: null,
          maxConcurrentRuns: 2,
          running: 0,
          queued: [
            {
              runId: SELF_RUN,
              agentId: SELF,
              issueId: "issue-self",
              routineOriginId: null,
              routine: false,
              createdAtMs: BASE,
              invocationSource: "on_demand",
              triggerDetail: "manual",
            },
          ],
        },
      },
      issues: {},
    };
    const { harness } = await setupHarness({
      seedAgents: [makeAgent(SELF)],
      persisted: { snapshot: staleSnapshot },
      route: () => {
        throw new Error("db down");
      },
    });
    const result = await gateCall(harness.ctx);
    expect(result).toEqual({});
  });

  it("GET /status returns the live ranking with deferrals", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    await gateCall(harness.ctx); // first consult fails open and triggers the refresh
    await waitFor(() =>
      Boolean(
        harness.getState({
          scopeKind: "company",
          scopeId: COMPANY,
          namespace: "wip-gate",
          stateKey: "snapshot",
        }),
      ),
    );
    // Now that the snapshot is warm, the consult defers and records it.
    const second = await gateCall(harness.ctx);
    expect(second.defer).toBe(true);
    const response = await apiCall(harness.ctx, { routeKey: "status", method: "GET" });
    expect(response.status).toBe(200);
    const body = response.body as {
      enabled: boolean;
      snapshot: { ranking: Array<{ runId: string; agentId: string; tier: number; blockingImpact: number }> };
      deferrals: Array<{ runId: string }>;
    };
    expect(body.enabled).toBe(true);
    expect(body.snapshot.ranking[0]?.runId).toBe(OTHER_RUN);
    expect(body.snapshot.ranking[1]?.runId).toBe(SELF_RUN);
    expect(body.deferrals.map((d) => d.runId)).toEqual([SELF_RUN]);
  });

  it("GET/PATCH /config round-trips through durable state", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    const patched = await apiCall(harness.ctx, {
      routeKey: "config",
      method: "PATCH",
      body: { enabled: false, ageOutMinutes: 9, weights: { [OTHER]: 2 } },
    });
    expect(patched.status).toBe(200);

    const persisted = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "config",
    }) as GateConfig;
    expect(persisted.enabled).toBe(false);
    expect(persisted.ageOutMinutes).toBe(9);
    expect(persisted.weights).toEqual({ [OTHER]: 2 });

    const fetched = await apiCall(harness.ctx, { routeKey: "config", method: "GET" });
    expect((fetched.body as { config: GateConfig }).config).toEqual(persisted);

    const disabled = await gateCall(harness.ctx);
    expect(disabled).toEqual({});
  });

  it("rejects malformed config patches", async () => {
    const { harness } = await setupHarness({});
    for (const body of [
      { enabled: "yes" },
      { ageOutMinutes: 0 },
      { weights: { [SELF]: 0 } },
      { weights: { [SELF]: "x" } },
      { routineOverrides: { r1: "middle" } },
      { stickyPausedAgents: [42] },
    ]) {
      const response = await apiCall(harness.ctx, { routeKey: "config", method: "PATCH", body });
      expect(response.status).toBe(400);
    }
  });

  it("POST /agent-override sticky-pause pauses the agent and records the sticky intent", async () => {
    const self = makeAgent(SELF);
    const { harness } = await setupHarness({ seedAgents: [self, makeAgent(OTHER)] });
    const response = await apiCall(harness.ctx, {
      routeKey: "agent-override",
      method: "POST",
      body: { agentId: SELF, action: "sticky-pause" },
    });
    expect(response.status).toBe(200);
    const agentAfter = await harness.ctx.agents.get(SELF, COMPANY);
    expect(agentAfter?.status).toBe("paused");
    const config = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "config",
    }) as GateConfig;
    expect(config.stickyPausedAgents).toEqual([SELF]);

    const resumed = await apiCall(harness.ctx, {
      routeKey: "agent-override",
      method: "POST",
      body: { agentId: SELF, action: "resume" },
    });
    expect(resumed.status).toBe(200);
    const agentResumed = await harness.ctx.agents.get(SELF, COMPANY);
    expect(agentResumed?.status).toBe("idle");
    const configAfter = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "config",
    }) as GateConfig;
    expect(configAfter.stickyPausedAgents).toEqual([]);
  });

  it("rejects unknown agent-override actions", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF)] });
    const response = await apiCall(harness.ctx, {
      routeKey: "agent-override",
      method: "POST",
      body: { agentId: SELF, action: "yell" },
    });
    expect(response.status).toBe(400);
  });

  it("reports tracked companies from onHealth", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    await gateCall(harness.ctx);
    const health = await plugin.definition.onHealth!();
    expect(health.status).toBe("ok");
    const details = health.details as { companies: Array<{ companyId: string }> };
    expect(details.companies.map((c) => c.companyId)).toContain(COMPANY);
  });

  it("reconcile job refreshes every known company without a company scope", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    await gateCall(harness.ctx);
    await waitFor(() =>
      Boolean(
        harness.getState({
          scopeKind: "company",
          scopeId: COMPANY,
          namespace: "wip-gate",
          stateKey: "snapshot",
        }),
      ),
    );
    await harness.runJob("reconcile");
    const snapshot = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "snapshot",
    }) as CompanySnapshot;
    expect(snapshot.refreshedAtMs).toBeGreaterThan(0);
  });

  it("event handlers refresh the company snapshot", async () => {
    const { harness } = await setupHarness({ seedAgents: [makeAgent(SELF), makeAgent(OTHER)] });
    await harness.emit("issue.relations.updated", { issueId: "issue-other" }, { companyId: COMPANY });
    // The handler schedules the refresh in the background.
    await waitFor(() =>
      Boolean(
        harness.getState({
          scopeKind: "company",
          scopeId: COMPANY,
          namespace: "wip-gate",
          stateKey: "snapshot",
        }),
      ),
    );
    const snapshot = harness.getState({
      scopeKind: "company",
      scopeId: COMPANY,
      namespace: "wip-gate",
      stateKey: "snapshot",
    }) as CompanySnapshot;
    expect(snapshot.companyId).toBe(COMPANY);
  });
});
