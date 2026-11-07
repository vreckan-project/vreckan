import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { eq, sql } from "drizzle-orm";
import {
  agentWakeupRequests,
  agents,
  authUsers,
  companies,
  createDb,
  heartbeatRuns,
  issueRelations,
  issues,
  pluginDatabaseNamespaces,
  plugins,
  getEmbeddedPostgresTestSupport,
  startEmbeddedPostgresTestDatabase,
} from "@paperclipai/db";
import { createPluginEventBus } from "../services/plugin-event-bus.js";
import { setPluginEventBus } from "../services/activity-log.js";
import { buildHostServices } from "../services/plugin-host-services.js";
import { derivePluginDatabaseNamespace } from "../services/plugin-database.js";
import {
  createPluginWorkerManager,
  type PluginWorkerHandle,
  type PluginWorkerManager,
} from "../services/plugin-worker-manager.js";
import { heartbeatService } from "../services/heartbeat.ts";
import { schedulingGate } from "../services/heartbeat/scheduling-gate.js";
import { createHostClientHandlers } from "@paperclipai/plugin-sdk";
import pluginManifest from "../../../packages/plugins/global-wip-gate/src/manifest.js";
import type { PaperclipPluginManifestV1, PluginApiRequestInput } from "@paperclipai/shared";

vi.mock("../telemetry.ts", async () => {
  const actual = await vi.importActual<typeof import("../telemetry.ts")>("../telemetry.ts");
  return {
    ...actual,
    initTelemetry: vi.fn(),
    shutdownTelemetry: vi.fn(),
    captureException: vi.fn(),
    captureMessage: vi.fn(),
    setTelemetryUser: vi.fn(),
  };
});

vi.mock("../sentry.ts", async () => {
  const actual = await vi.importActual<typeof import("../sentry.ts")>("../sentry.ts");
  return {
    ...actual,
    initSentry: vi.fn(),
    captureException: vi.fn(),
    captureMessage: vi.fn(),
  };
});

/**
 * The central adapter dispatch is mocked so claimed runs execute through a
 * test stub instead of a real provider. The stub stays in flight for 150 ms
 * (long enough that two sequentially claimed runs overlap) and records the
 * peak concurrency so the host cap can be asserted exactly.
 */
const mockAdapterExecute = vi.fn<() => Promise<unknown>>();
vi.mock("../adapters/index.ts", async () => {
  const actual = await vi.importActual<typeof import("../adapters/index.ts")>(
    "../adapters/index.ts",
  );
  return {
    ...actual,
    getServerAdapter: vi.fn(() => ({
      supportsLocalAgentJwt: false,
      execute: mockAdapterExecute,
    })),
  };
});

const externalTestDatabaseUrl = process.env.PAPERCLIP_TEST_DATABASE_URL?.trim();
const embeddedPostgresSupport = externalTestDatabaseUrl
  ? { supported: true }
  : await getEmbeddedPostgresTestSupport();
const describeEmbeddedPostgres = embeddedPostgresSupport.supported
  ? describe
  : describe.skip;

if (!embeddedPostgresSupport.supported) {
  console.warn(
    `Skipping global-wip-gate integration tests on this host: ${embeddedPostgresSupport.reason ?? "unsupported environment"}`,
  );
}

// The host keys plugin state, plugin rows, and namespaces by UUID, so the
// test's plugin id must be a real UUID even though the worker manager treats
// it as an opaque key.
const uuid = (): string => randomUUID();
const PLUGIN_ID = uuid();
const PLUGIN_KEY = pluginManifest.id;
const PLUGIN_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../packages/plugins/global-wip-gate",
);
const WORKER_ENTRYPOINT = path.join(PLUGIN_DIR, "dist", "worker.js");

const manifest = pluginManifest as PaperclipPluginManifestV1;

function ensurePluginBuilt(): void {
  if (existsSync(WORKER_ENTRYPOINT)) return;
  execFileSync(process.execPath, ["./esbuild.config.mjs"], { cwd: PLUGIN_DIR, stdio: "inherit" });
}

let issueCounter = 0;
const nextIdentifier = (): string => `WG-${(issueCounter += 1)}`;

async function runStatus(db: ReturnType<typeof createDb>, runId: string): Promise<string | null> {
  const [row] = await db
    .select({ status: heartbeatRuns.status })
    .from(heartbeatRuns)
    .where(eq(heartbeatRuns.id, runId));
  return row?.status ?? null;
}

async function apiCall(
  worker: PluginWorkerHandle,
  companyId: string,
  routeKey: string,
  method: string,
  body: Record<string, unknown> = {},
  query: Record<string, string> = { companyId },
) {
  const input: PluginApiRequestInput = {
    routeKey,
    method,
    path: "",
    params: {},
    query,
    body,
    actor: {
      actorType: "user",
      actorId: "integration-user",
      agentId: null,
      userId: "integration-user",
      runId: null,
    },
    companyId,
    headers: {},
  };
  return worker.call("handleApiRequest", input, 5_000) as Promise<{
    status?: number;
    body?: unknown;
  }>;
}

async function waitForWorkerRunning(manager: PluginWorkerManager, timeoutMs = 15_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const handle = manager.getWorker(PLUGIN_ID);
    if (handle && handle.status === "running") return;
    if (Date.now() > deadline) {
      throw new Error(`plugin worker did not reach running in time (status: ${handle?.status ?? "none"})`);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

/** Await a run reaching a terminal status. */
async function waitForTerminal(
  db: ReturnType<typeof createDb>,
  runId: string,
  timeoutMs = 15_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const status = await runStatus(db, runId);
    if (status && !["queued", "running"].includes(status)) return status;
    if (Date.now() > deadline) {
      throw new Error(`run ${runId} did not reach a terminal status (last: ${status})`);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

/**
 * Barrier on the gate's own counters. The execution settle tail (lease stop,
 * trace finalization, gate release) lands after the run row turns terminal,
 * so asserting `inFlightCount === 0` directly after a terminal status races;
 * a stale count would also show up as zero remaining slots for the next
 * claim pass in a later test.
 */
async function waitForGateSettled(companyId: string, timeoutMs = 15_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (
      schedulingGate.inFlightCount(companyId) === 0 &&
      schedulingGate.reservedCount(companyId) === 0
    ) {
      return;
    }
    if (Date.now() > deadline) {
      throw new Error(
        `scheduling gate did not settle for ${companyId} (inFlight=${schedulingGate.inFlightCount(companyId)}, reserved=${schedulingGate.reservedCount(companyId)})`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

describeEmbeddedPostgres("global-wip-gate plugin on the scheduling gate", () => {
  let db!: ReturnType<typeof createDb>;
  let tempDb: Awaited<
    ReturnType<typeof startEmbeddedPostgresTestDatabase>
  > | null = null;
  let workerManager: PluginWorkerManager | null = null;
  let worker: PluginWorkerHandle | null = null;
  let activeExecutions = 0;
  let maxActiveExecutions = 0;

  const companyId = uuid();
  let scenario!: {
    runs: Record<string, { runId: string; agentId: string; issueId: string }>;
  };

  async function seedAgentWithQueuedRun(input: {
    name: string;
    status: string;
    issueId: string;
    createdOffsetMs: number;
    now: Date;
  }): Promise<{ runId: string; agentId: string; issueId: string }> {
    const agentId = uuid();
    const runId = uuid();
    const wakeupRequestId = uuid();
    const createdAt = new Date(input.now.getTime() + input.createdOffsetMs);
    await db.insert(agents).values({
      id: agentId,
      companyId,
      name: input.name,
      role: "engineer",
      title: null,
      status: input.status,
      reportsTo: null,
      adapterType: "codex_local",
      adapterConfig: {},
      runtimeConfig: {},
      permissions: {},
    });
    // The execution gate cancels a claimed run whose issue is not assigned
    // to the running agent (issue_assignee_changed), so the run's issue must
    // be owned by the agent it runs on.
    await db
      .update(issues)
      .set({ assigneeAgentId: agentId })
      .where(eq(issues.id, input.issueId));
    await db.insert(agentWakeupRequests).values({
      id: wakeupRequestId,
      companyId,
      agentId,
      source: "on_demand",
      triggerDetail: "manual",
      reason: "test",
      payload: { issueId: input.issueId },
      status: "pending",
      runId: null,
    });
    await db.insert(heartbeatRuns).values({
      id: runId,
      companyId,
      agentId,
      invocationSource: "on_demand",
      triggerDetail: "manual",
      status: "queued",
      wakeupRequestId,
      contextSnapshot: { issueId: input.issueId },
      issueId: input.issueId,
      createdAt,
      updatedAt: createdAt,
    });
    return { runId, agentId, issueId: input.issueId };
  }

  async function seedIssue(input: {
    title: string;
    status: string;
    priority: string;
    originKind?: string;
    originId?: string | null;
  }): Promise<string> {
    const issueId = uuid();
    await db.insert(issues).values({
      id: issueId,
      companyId,
      title: input.title,
      status: input.status,
      priority: input.priority,
      originKind: input.originKind ?? "manual",
      originId: input.originId ?? null,
      assigneeAgentId: null,
      responsibleUserId: "integration-user",
      issueNumber: issueCounter,
      identifier: nextIdentifier(),
    });
    return issueId;
  }

  async function seedBlockedLeaf(impactIssueId: string, title: string): Promise<void> {
    const leaf = uuid();
    await db.insert(issues).values({
      id: leaf,
      companyId,
      title,
      status: "backlog",
      priority: "low",
      originKind: "manual",
      originId: null,
      assigneeAgentId: null,
      responsibleUserId: "integration-user",
      issueNumber: issueCounter,
      identifier: nextIdentifier(),
    });
    await db.insert(issueRelations).values({
      companyId,
      issueId: impactIssueId,
      relatedIssueId: leaf,
      type: "blocks",
    });
  }

  beforeAll(async () => {
    mockAdapterExecute.mockImplementation((ctx: { runId?: string; context?: Record<string, unknown> }) => {
      activeExecutions += 1;
      maxActiveExecutions = Math.max(maxActiveExecutions, activeExecutions);
      // Record the disposition a real agent would leave behind: mark the
      // run's issue done while the run is in flight. That terminates both
      // post-success automation paths — the liveness continuation only fires
      // for todo/in_progress issues, and the legacy disposition repair is
      // skipped for issues that already carry a recorded disposition — so
      // each seeded run settles as exactly one terminal row.
      const issueId = typeof ctx?.context?.issueId === "string" ? ctx.context.issueId : null;
      if (issueId) {
        void db
          .update(issues)
          .set({ status: "done" })
          .where(eq(issues.id, issueId))
          .catch(() => null);
      }
      return new Promise<unknown>((resolve) =>
        setTimeout(
          () => {
            activeExecutions -= 1;
            resolve({
              exitCode: 0,
              signal: null,
              timedOut: false,
              errorMessage: null,
              summary: "integration test run",
              provider: "test",
              model: "test-model",
            });
          },
          150,
        ),
      );
    });

    if (externalTestDatabaseUrl) {
      db = createDb(externalTestDatabaseUrl);
    } else {
      tempDb = await startEmbeddedPostgresTestDatabase("paperclip-global-wip-gate-");
      db = createDb(tempDb.connectionString);
    }

    // Register the plugin the way an install would: a plugins row (UUID id,
    // the real manifest as manifest_json so db.query whitelisting sees the
    // declared core read tables) and an active database namespace.
    const namespaceName = derivePluginDatabaseNamespace(manifest.id);
    await db.insert(plugins).values({
      id: PLUGIN_ID,
      pluginKey: manifest.id,
      packageName: "@paperclipai/global-wip-gate",
      version: manifest.version,
      apiVersion: manifest.apiVersion,
      categories: manifest.categories,
      manifestJson: manifest,
      status: "installed",
      installOrder: 1,
    });
    await db.execute(sql`CREATE SCHEMA IF NOT EXISTS ${sql.raw(namespaceName)}`).catch(() => null);
    await db.insert(pluginDatabaseNamespaces).values({
      pluginId: PLUGIN_ID,
      pluginKey: manifest.id,
      namespaceName,
      namespaceMode: "schema",
      status: "active",
    });

    const now = new Date();
    await db.insert(authUsers).values({
      id: "integration-user",
      name: "Integration User",
      email: "integration-user@example.test",
      emailVerified: true,
      createdAt: now,
      updatedAt: now,
    });
    // The company's host gate: enabled, cap of 2.
    await db.insert(companies).values({
      id: companyId,
      name: "WIP Gate Co",
      issuePrefix: "WG",
      defaultResponsibleUserId: "integration-user",
      requireBoardApprovalForNewAgents: false,
      scheduling: { enabled: true, maxInFlight: 2 },
    });

    // Scenario: one queued run per agent.
    //  - routine:     routine-execution run (ranks LAST by policy)
    //  - medium:      medium priority, no impact
    //  - critical:    high priority, transitively blocks 4 issues
    //  - paused-high: paused, high priority, blocks 4 issues (not claimable)
    const routineIssue = await seedIssue({
      title: "Routine standup",
      status: "todo",
      priority: "medium",
      originKind: "routine_execution",
      originId: uuid(),
    });
    const mediumIssue = await seedIssue({
      title: "Medium work",
      status: "todo",
      priority: "medium",
    });
    const criticalIssue = await seedIssue({
      title: "Critical unblocker",
      status: "todo",
      priority: "high",
    });
    const pausedIssue = await seedIssue({
      title: "Paused agent blocker",
      status: "todo",
      priority: "high",
    });
    const blocked = [
      await seedIssue({ title: "Blocked 1", status: "backlog", priority: "low" }),
      await seedIssue({ title: "Blocked 2", status: "backlog", priority: "low" }),
      await seedIssue({ title: "Blocked 3", status: "backlog", priority: "low" }),
      await seedIssue({ title: "Blocked 4", status: "backlog", priority: "low" }),
    ];
    for (const leaf of blocked.slice(0, 3)) {
      await db.insert(issueRelations).values({
        companyId,
        issueId: criticalIssue,
        relatedIssueId: leaf,
        type: "blocks",
      });
    }
    await db.insert(issueRelations).values({
      companyId,
      issueId: blocked[0]!,
      relatedIssueId: blocked[3]!,
      type: "blocks",
    });
    for (const leaf of blocked) {
      await db.insert(issueRelations).values({
        companyId,
        issueId: pausedIssue,
        relatedIssueId: leaf,
        type: "blocks",
      });
    }

    // Seed the runs BEFORE the worker starts so the worker's first refresh
    // sees the whole company.
    const seedNow = new Date();
    const runs: Record<string, { runId: string; agentId: string; issueId: string }> = {};
    runs.routine = await seedAgentWithQueuedRun({
      name: "routine",
      status: "idle",
      issueId: routineIssue,
      createdOffsetMs: 0,
      now: seedNow,
    });
    runs.medium = await seedAgentWithQueuedRun({
      name: "medium",
      status: "idle",
      issueId: mediumIssue,
      createdOffsetMs: 1_000,
      now: seedNow,
    });
    runs.critical = await seedAgentWithQueuedRun({
      name: "critical",
      status: "idle",
      issueId: criticalIssue,
      createdOffsetMs: 2_000,
      now: seedNow,
    });
    runs["paused-high"] = await seedAgentWithQueuedRun({
      name: "paused-high",
      status: "paused",
      issueId: pausedIssue,
      createdOffsetMs: 3_000,
      now: seedNow,
    });
    scenario = { runs };

    // Wire the process-global domain event publisher to the same bus the
    // plugin worker subscribes on, so agent.run.* lifecycle events reach the
    // worker and keep its snapshot current.
    const eventBus = createPluginEventBus();
    setPluginEventBus(eventBus);
    workerManager = createPluginWorkerManager();
    const notifyWorker = (method: string, params: unknown): void => {
      const handle = workerManager?.getWorker(PLUGIN_ID);
      if (handle) handle.notify(method, params);
    };
    const services = buildHostServices(db, PLUGIN_ID, PLUGIN_KEY, eventBus, notifyWorker, {
      pluginWorkerManager: workerManager,
      manifest,
    });
    const hostHandlers = createHostClientHandlers({
      pluginId: PLUGIN_ID,
      capabilities: manifest.capabilities,
      services,
    });
    ensurePluginBuilt();
    worker = await workerManager.startWorker(PLUGIN_ID, {
      entrypointPath: WORKER_ENTRYPOINT,
      manifest,
      config: {},
      instanceInfo: { instanceId: "global-wip-gate-it", hostVersion: "test" },
      apiVersion: 1,
      databaseNamespace: namespaceName,
      hostHandlers,
      autoRestart: false,
      env: {},
      // The production install flow seeds the plugin's configured companies
      // here so no-invocation (job/timer) state writes are authorized.
      proactiveCompanyScopes: [companyId],
    });
    await waitForWorkerRunning(workerManager);

    // Warm the worker: hydrate durable state, then run a full refresh so the
    // first gate consult in the tests is decision-capable.
    const warm = await apiCall(worker, companyId, "status", "GET", {}, { companyId, refresh: "1" });
    expect(warm.status).toBe(200);
    const warmBody = warm.body as { snapshot: { agents: unknown[] } | null };
    expect(warmBody.snapshot?.agents).toHaveLength(4);

    // Fresh gate state: no in-flight, no cached config. Test passes rebind
    // the database and worker manager through heartbeatService.
    schedulingGate.reset();
  }, 90_000);

  afterAll(async () => {
    if (workerManager) {
      await workerManager.stopAll();
    }
    if (tempDb) await tempDb.cleanup();
  }, 30_000);

  it("routes scarce slots by ranking: routines last, blocking-impact first, paused agents excluded", async () => {
    const heartbeat = heartbeatService(db, { pluginWorkerManager: workerManager!, runtimeEnv: {} });
    const { routine, medium, critical, "paused-high": pausedHigh } = scenario.runs;

    // Adversarial sweep: the lowest-ranked claimable agent asks first.
    const routinePass = await heartbeat.resumeQueuedRunsForAgent(companyId, routine.agentId);
    expect(routinePass).toEqual([]);
    expect(await runStatus(db, routine.runId)).toBe("queued");
    const deferDecision = (await schedulingGate.status(companyId)).lastDecision;
    expect(deferDecision?.outcome).toBe("plugin_defer");
    expect(deferDecision?.plugin?.pluginId).toBe(PLUGIN_ID);
    expect(deferDecision?.plugin?.reason).toMatch(/higher-ranked/);

    // The medium agent ranks below critical but above routine: only one
    // superior exists and two slots remain, so it claims instead of being
    // deferred.
    const mediumPass = await heartbeat.resumeQueuedRunsForAgent(companyId, medium.agentId);
    expect(mediumPass).toHaveLength(1);
    expect(await runStatus(db, medium.runId)).not.toBe("queued");

    // The critical agent is the top-ranked run in the company: it claims the
    // remaining slot even while the medium run is still in flight.
    const criticalPass = await heartbeat.resumeQueuedRunsForAgent(companyId, critical.agentId);
    expect(criticalPass).toHaveLength(1);
    expect(await runStatus(db, critical.runId)).not.toBe("queued");

    // The paused agent's run is the highest-impact work in the company, but a
    // paused agent is not claimable: it neither claims nor blocks anyone.
    const pausedPass = await heartbeat.resumeQueuedRunsForAgent(companyId, pausedHigh.agentId);
    expect(pausedPass).toEqual([]);
    expect(await runStatus(db, pausedHigh.runId)).toBe("queued");

    // The deferred routine run is still queued: the scarce slots landed with
    // the two highest-ranked claimable agents.
    expect(await runStatus(db, routine.runId)).toBe("queued");
    expect(await runStatus(db, medium.runId)).not.toBe("queued");
    expect(await runStatus(db, critical.runId)).not.toBe("queued");
  }, 60_000);

  it("gives the deferred routine run a slot once in-flight work finishes", async () => {
    const heartbeat = heartbeatService(db, { pluginWorkerManager: workerManager!, runtimeEnv: {} });
    const { routine, medium, critical, "paused-high": pausedHigh } = scenario.runs;

    for (const runId of [medium.runId, critical.runId]) {
      expect(await waitForTerminal(db, runId)).toBe("succeeded");
    }
    await waitForGateSettled(companyId);

    // Force a fresh worker snapshot (medium/critical are no longer queued),
    // then the routine agent is the top-ranked queued work: it claims.
    const barrier = await apiCall(worker!, companyId, "status", "GET", {}, { companyId, refresh: "1" });
    expect(barrier.status).toBe(200);
    const pass = await heartbeat.resumeQueuedRunsForAgent(companyId, routine.agentId);
    expect(pass).toHaveLength(1);
    expect(await runStatus(db, routine.runId)).not.toBe("queued");
    expect(await waitForTerminal(db, routine.runId)).toBe("succeeded");
    await waitForGateSettled(companyId);

    // Nothing was stranded except the paused agent's run: every claimable
    // agent's work reached a terminal state.
    const rows = await db
      .select({ status: heartbeatRuns.status })
      .from(heartbeatRuns)
      .where(eq(heartbeatRuns.companyId, companyId));
    expect(rows).toHaveLength(4);
    const pausedRow = rows.find((row) => row.status === "queued");
    expect(pausedRow?.status).toBe("queued");
    expect(await runStatus(db, pausedHigh.runId)).toBe("queued");
    for (const row of rows) {
      if (row.status !== "queued") {
        expect(["succeeded", "failed", "cancelled"]).toContain(row.status);
      }
    }
    // The cap was never exceeded: at most maxInFlight runs were in flight at
    // any instant.
    expect(maxActiveExecutions).toBeLessThanOrEqual(2);
    expect(maxActiveExecutions).toBeGreaterThanOrEqual(2);
  }, 60_000);

  it("reverts to FIFO with the cap intact when the plugin is disabled", async () => {
    const patch = await apiCall(worker!, companyId, "config", "PATCH", { enabled: false });
    expect(patch.status).toBe(200);

    const routine2 = await seedAgentWithQueuedRun({
      name: "routine2",
      status: "idle",
      issueId: await seedIssue({ title: "Routine 2", status: "todo", priority: "medium", originKind: "routine_execution", originId: uuid() }),
      createdOffsetMs: 0,
      now: new Date(),
    });
    const impact2 = await seedAgentWithQueuedRun({
      name: "impact2",
      status: "idle",
      issueId: await seedIssue({ title: "Impact 2", status: "todo", priority: "high" }),
      createdOffsetMs: 1_000,
      now: new Date(),
    });
    const impact2Issue = impact2.issueId;
    for (let i = 0; i < 3; i += 1) {
      await seedBlockedLeaf(impact2Issue, `leaf2 ${i}`);
    }

    const barrier = await apiCall(worker!, companyId, "status", "GET", {}, { companyId, refresh: "1" });
    expect(barrier.status).toBe(200);

    const heartbeat = heartbeatService(db, { pluginWorkerManager: workerManager!, runtimeEnv: {} });
    // Worst-first sweep: with the plugin disabled no deferral is possible, so
    // the routine agent's run claims a slot under host FIFO even though
    // impact2's run would outrank it.
    const routinePass = await heartbeat.resumeQueuedRunsForAgent(companyId, routine2.agentId);
    expect(routinePass).toHaveLength(1);
    const impactPass = await heartbeat.resumeQueuedRunsForAgent(companyId, impact2.agentId);
    expect(impactPass).toHaveLength(1);
    expect(await waitForTerminal(db, routine2.runId)).toBe("succeeded");
    expect(await waitForTerminal(db, impact2.runId)).toBe("succeeded");
    await waitForGateSettled(companyId);
    expect(maxActiveExecutions).toBeLessThanOrEqual(2);

    // Re-enable for the final phase.
    const reenabled = await apiCall(worker!, companyId, "config", "PATCH", { enabled: true });
    expect(reenabled.status).toBe(200);
  }, 60_000);

  it("fails open with FIFO ordering and an exact cap when the plugin worker dies", async () => {
    await workerManager!.stopWorker(PLUGIN_ID);
    expect(workerManager!.getWorker(PLUGIN_ID)?.status === "running").toBe(false);

    const routine3 = await seedAgentWithQueuedRun({
      name: "routine3",
      status: "idle",
      issueId: await seedIssue({ title: "Routine 3", status: "todo", priority: "medium", originKind: "routine_execution", originId: uuid() }),
      createdOffsetMs: 0,
      now: new Date(),
    });
    const impact3 = await seedAgentWithQueuedRun({
      name: "impact3",
      status: "idle",
      issueId: await seedIssue({ title: "Impact 3", status: "todo", priority: "high" }),
      createdOffsetMs: 1_000,
      now: new Date(),
    });
    for (let i = 0; i < 3; i += 1) {
      await seedBlockedLeaf(impact3.issueId, `leaf3 ${i}`);
    }

    const heartbeat = heartbeatService(db, { pluginWorkerManager: workerManager!, runtimeEnv: {} });
    // Worst-first sweep with the worker stopped: the host consultation finds
    // no running worker (fail-open), so the routine run claims its slot.
    const routinePass = await heartbeat.resumeQueuedRunsForAgent(companyId, routine3.agentId);
    expect(routinePass).toHaveLength(1);
    const impactPass = await heartbeat.resumeQueuedRunsForAgent(companyId, impact3.agentId);
    expect(impactPass).toHaveLength(1);
    expect(await waitForTerminal(db, routine3.runId)).toBe("succeeded");
    expect(await waitForTerminal(db, impact3.runId)).toBe("succeeded");
    await waitForGateSettled(companyId);
    expect(maxActiveExecutions).toBeLessThanOrEqual(2);
  }, 60_000);
});
