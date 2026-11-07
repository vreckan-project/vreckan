import type { Db } from "@paperclipai/db";
import {
  agents,
  agentWakeupRequests,
  companies,
  createDb,
  EMBEDDED_POSTGRES_TEST_TIMEOUT_MS,
  heartbeatRuns,
  issues,
} from "@paperclipai/db";
import { and, eq, sql } from "drizzle-orm";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { getEmbeddedPostgresTestSupport, startEmbeddedPostgresTestDatabase } from "../../__tests__/helpers/embedded-postgres.js";
import { createRunDispatch } from "../../modules/run-dispatch/index.js";
import type { createWakeQueue } from "../../modules/wake-queue/index.js";
import type { PluginWorkerHandle, PluginWorkerManager } from "../plugin-worker-manager.js";
import { createHeartbeatRunState } from "./run-state.js";
import { createHeartbeatRunPreparation } from "./run-preparation.js";
import { createHeartbeatQueue, type HeartbeatQueueDependencies } from "./queue.js";
import {
  SCHEDULING_GATE_CONSULT_TIMEOUT_MS,
  schedulingGate,
  validateSchedulingConfigPatch,
  SchedulingGate,
  type SchedulingGateConsultParams,
  type SchedulingGatePluginDescriptor,
} from "./scheduling-gate.js";

const support = await getEmbeddedPostgresTestSupport();
if (!support.supported) console.warn(`Skipping scheduling gate tests: ${support.reason}`);

type Run = typeof heartbeatRuns.$inferSelect;

const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

function fakeWorker(
  pluginId: string,
  options: {
    status?: PluginWorkerHandle["status"];
    supportedMethods?: string[];
    call?: PluginWorkerHandle["call"];
  } = {},
): PluginWorkerHandle {
  return {
    pluginId,
    status: options.status ?? "running",
    supportedMethods: options.supportedMethods ?? [],
    call: options.call ?? (vi.fn(async () => ({})) as PluginWorkerHandle["call"]),
  } as unknown as PluginWorkerHandle;
}

function fakeManager(workers: PluginWorkerHandle[]): PluginWorkerManager {
  return {
    listWorkers: () => workers,
  } as unknown as PluginWorkerManager;
}

function gateCallbacks(db: Db, overrides: Partial<HeartbeatQueueDependencies> = {}) {
  const state = createHeartbeatRunState(db);
  const preparation = createHeartbeatRunPreparation(db);
  return {
    ...preparation,
    ...state,
    options: {},
    setRunStatus: vi.fn<HeartbeatQueueDependencies["setRunStatus"]>(async (id, status, patch) =>
      db.update(heartbeatRuns).set({ ...patch, status }).where(eq(heartbeatRuns.id, id)).returning().then(rows => rows[0] ?? null),
    ),
    setWakeupStatus: vi.fn<HeartbeatQueueDependencies["setWakeupStatus"]>(async () => undefined),
    appendRunEvent: vi.fn<HeartbeatQueueDependencies["appendRunEvent"]>(async () => undefined),
    finalizeAgentStatus: vi.fn(async () => undefined),
    getAgentInvokability: vi.fn<HeartbeatQueueDependencies["getAgentInvokability"]>(async () => ({ invokable: true })),
    getWorktreeExecutionCutoff: vi.fn(async () => null),
    applyRunDispatchPostCommitEffects: vi.fn(),
    issuesSvc: {
      getDependencyReadiness: vi.fn<HeartbeatQueueDependencies["issuesSvc"]["getDependencyReadiness"]>(),
      listDependencyReadiness: vi.fn(async () => new Map()),
    },
    treeControlSvc: { getActivePauseHoldGate: vi.fn(async () => null) },
    budgets: { getInvocationBlock: vi.fn(async () => null) },
    instanceSettings: {
      getExperimental: vi.fn(async () => ({})) as unknown as HeartbeatQueueDependencies["instanceSettings"]["getExperimental"],
    },
    runDispatch: createRunDispatch(db),
    wakeQueue: {
      createAdmissionTransactionScope: vi.fn<ReturnType<typeof createWakeQueue>["createAdmissionTransactionScope"]>(),
      admitWakeBehindIssueExecution: vi.fn<ReturnType<typeof createWakeQueue>["admitWakeBehindIssueExecution"]>(),
    },
    activeRunExecutions: new Set<string>(),
    activeRunExecutionPromises: new Set<Promise<void>>(),
    activeWakeupPromises: new Set<Promise<unknown>>(),
    liveRunExecutions: { has: vi.fn(() => false) },
    isHeartbeatRunTerminalStatus: (status: string | null | undefined) => ["succeeded", "failed", "cancelled", "timed_out"].includes(status ?? ""),
    isSameTaskScope: (left: string | null, right: string | null) => left === right,
    runTaskKey: (run: Run) => typeof run.contextSnapshot?.issueId === "string" ? run.contextSnapshot.issueId : null,
    filterZombieCoalesceTarget: <T extends { id: string; status: string }>(run: T | null) => run,
    getSchedulingSuppression: vi.fn(async () => ({ suppressed: false, reason: null })),
    executeRun: vi.fn(async () => undefined),
    applyWakeQueuePostCommitEffects: vi.fn(async () => undefined),
    releaseIssueExecutionAndPromote: vi.fn(async () => undefined),
    publishRunLifecyclePluginEvent: vi.fn(),
    cancelRunInternal: vi.fn(async () => undefined),
    cancelActiveForAgentInternal: vi.fn(async () => undefined),
    resumeExecutionWaitComments: vi.fn(async () => undefined),
    resumeQueuedCommentInterrupt: vi.fn(async () => undefined),
    resumeSavedLegacyComments: vi.fn(async () => undefined),
    formatIssueIdentifierLink: (_identifier: string | null, fallback: string) => fallback,
    sweepPendingCleanupLeases: vi.fn<HeartbeatQueueDependencies["sweepPendingCleanupLeases"]>(),
    ...overrides,
  } satisfies HeartbeatQueueDependencies;
}

// Claim passes call executeRun fire-and-forget and track the promise in
// activeRunExecutionPromises; the pass resolves before execution starts.
// Drain the set so start-site mirrors have run before asserting on counters.
async function drainExecutions(callbacks: ReturnType<typeof gateCallbacks>, maxLoops = 10): Promise<void> {
  for (let i = 0; i < maxLoops && callbacks.activeRunExecutionPromises.size > 0; i += 1) {
    await Promise.allSettled([...callbacks.activeRunExecutionPromises]);
  }
}

describe.skipIf(!support.supported)("scheduling gate", () => {
  let database: Awaited<ReturnType<typeof startEmbeddedPostgresTestDatabase>>;
  let db: Db;
  const gate = schedulingGate;

  beforeAll(async () => {
    database = await startEmbeddedPostgresTestDatabase("paperclip-scheduling-gate");
    db = createDb(database.connectionString);
  }, EMBEDDED_POSTGRES_TEST_TIMEOUT_MS);

  afterAll(async () => {
    await db?.$client.end();
    await database?.cleanup();
  });

  beforeEach(async () => {
    gate.reset();
    if (db) await db.execute(sql`truncate table companies restart identity cascade`);
  });

  async function fixtureCompany(name: string) {
    const [company] = await db
      .insert(companies)
      .values({ name, issuePrefix: `G${Math.floor(Math.random() * 9000) + 1000}` })
      .returning();
    return company;
  }

  async function fixtureAgent(companyId: string, name: string) {
    const [agent] = await db
      .insert(agents)
      .values({
        companyId,
        name,
        role: "engineer",
        status: "idle",
        adapterType: "process",
        adapterConfig: {},
        runtimeConfig: { heartbeat: { maxConcurrentRuns: 2 } },
      })
      .returning();
    return agent;
  }

  async function fixtureQueuedRun(companyId: string, agent: typeof agents.$inferSelect, title: string) {
    const [issue] = await db
      .insert(issues)
      .values({ companyId, title, status: "in_progress", assigneeAgentId: agent.id })
      .returning();
    const [wake] = await db
      .insert(agentWakeupRequests)
      .values({ companyId, agentId: agent.id, source: "assignment", status: "queued" })
      .returning();
    const [run] = await db
      .insert(heartbeatRuns)
      .values({
        companyId,
        agentId: agent.id,
        status: "queued",
        invocationSource: "assignment",
        wakeupRequestId: wake.id,
        // The wake-seed path persists the responsible user on the run row;
        // the claim re-resolves it and rejects the run without one.
        responsibleUserId: "board-responsible",
        contextSnapshot: { issueId: issue.id },
      })
      .returning();
    return { company: { id: companyId }, agent, issue, wake, run };
  }

  describe("in-memory counters and reservation", () => {
    const COMPANY = "company-burst";

    it("holds inFlight + reserved <= maxInFlight under a concurrent claim burst across 6 agents", async () => {
      // No db bound: the gate resolves the default config (enabled, 8).
      const seen: number[] = [];
      const invariant = () => {
        const total = gate.inFlightCount(COMPANY) + gate.reservedCount(COMPANY);
        seen.push(total);
        expect(total).toBeLessThanOrEqual(8);
      };
      const claimPass = async (agent: string) => {
        // Simulate the queue claim pass: async config/remaining read, then the
        // synchronous reservation, with yields between steps so other passes
        // interleave on the event loop (per-agent locks do not serialize
        // across agents).
        const remaining = await gate.remainingSlots(COMPANY);
        await tick();
        const reserved = gate.reserve(COMPANY, Math.min(2, remaining));
        invariant();
        for (let i = 0; i < reserved; i += 1) {
          const runId = `${agent}-run-${i}`;
          gate.markClaimedRun(runId, COMPANY);
          await tick();
          gate.onExecutionStarted(runId, COMPANY);
          invariant();
        }
        gate.release(COMPANY, Math.max(0, reserved));
        return reserved;
      };
      const reservedPerAgent = await Promise.all(
        ["a1", "a2", "a3", "a4", "a5", "a6"].map(claimPass),
      );
      // 12 candidate slots (6 agents x 2) against a cap of 8: exactly 8 are
      // reserved across the burst, four agents claim nothing.
      expect(reservedPerAgent.reduce((sum, n) => sum + n, 0)).toBe(8);
      expect(gate.inFlightCount(COMPANY)).toBe(8);
      expect(gate.reservedCount(COMPANY)).toBe(0);
      expect(seen.every((total) => total <= 8)).toBe(true);
      // Finish all eight runs: the counters drain back to zero.
      for (const agent of ["a1", "a2", "a3", "a4", "a5", "a6"]) {
        for (let i = 0; i < reservedPerAgent[["a1", "a2", "a3", "a4", "a5", "a6"].indexOf(agent)]; i += 1) {
          gate.onExecutionFinished(`${agent}-run-${i}`);
          invariant();
        }
      }
      expect(gate.inFlightCount(COMPANY)).toBe(0);
    });

    it("releases reservations when claimed runs are cancelled before start", async () => {
      gate.bind(db);
      const companyId = (await fixtureCompany("Cancel Co")).id;
      const reserved = gate.reserve(companyId, 3);
      expect(reserved).toBe(3);
      gate.markClaimedRun("run-1", companyId);
      gate.markClaimedRun("run-2", companyId);
      gate.markClaimedRun("run-3", companyId);
      expect(gate.reservedCount(companyId)).toBe(3);
      // Claim pass reserved 3 but only claims 2: the unused slot is released.
      gate.release(companyId, 1);
      expect(gate.reservedCount(companyId)).toBe(2);
      // run-3 dies before the start site: its slot is released.
      gate.releaseClaimedRun("run-3");
      expect(gate.reservedCount(companyId)).toBe(1);
      // run-1 starts (conversion), then the release is a no-op.
      gate.onExecutionStarted("run-1", companyId);
      expect(gate.inFlightCount(companyId)).toBe(1);
      expect(gate.reservedCount(companyId)).toBe(0);
      gate.releaseClaimedRun("run-1");
      expect(gate.reservedCount(companyId)).toBe(0);
      expect(gate.inFlightCount(companyId)).toBe(1);
      gate.onExecutionFinished("run-1");
      expect(gate.inFlightCount(companyId)).toBe(0);
    });

    it("counts direct starts (queue bypass) without double-counting re-executions", () => {
      const companyId = "company-direct";
      gate.onExecutionStarted("run-x", companyId);
      expect(gate.inFlightCount(companyId)).toBe(1);
      gate.onExecutionStarted("run-x", companyId);
      expect(gate.inFlightCount(companyId)).toBe(1);
      gate.onExecutionFinished("run-x");
      expect(gate.inFlightCount(companyId)).toBe(0);
      gate.onExecutionFinished("run-x");
      expect(gate.inFlightCount(companyId)).toBe(0);
    });

    it("a second interleaved pass sees the first pass's reservation (no over-reserve)", async () => {
      // Both passes read the remainder before either reserves — the exact
      // cross-agent burst window the reservation exists to close.
      const remainingA = await gate.remainingSlots("company-two");
      await tick();
      const remainingB = await gate.remainingSlots("company-two");
      expect(remainingA).toBe(8);
      expect(remainingB).toBe(8);
      const reservedA = gate.reserve("company-two", Math.min(2, remainingA));
      const reservedB = gate.reserve("company-two", Math.min(4, remainingB));
      expect(reservedA).toBe(2);
      // Pass B's reservation is clamped by pass A's hold: 4 requested, only
      // 6 left under the cap of 8.
      expect(reservedB).toBe(4);
      expect(gate.reservedCount("company-two")).toBe(6);
      // Convert everything to in-flight, then finish it: no counter leaks.
      for (let i = 0; i < reservedA + reservedB; i += 1) {
        const id = `run-${i}`;
        gate.markClaimedRun(id, "company-two");
        gate.onExecutionStarted(id, "company-two");
      }
      expect(gate.inFlightCount("company-two")).toBe(reservedA + reservedB);
      expect(gate.reservedCount("company-two")).toBe(0);
      for (let i = 0; i < reservedA + reservedB; i += 1) gate.onExecutionFinished(`run-${i}`);
      expect(gate.inFlightCount("company-two")).toBe(0);
    });
  });

  describe("config persistence", () => {
    it("defaults to enabled with maxInFlight 8 when no config row exists", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Default Co");
      const config = await gate.configFor(company.id);
      expect(config).toEqual({ enabled: true, maxInFlight: 8 });
      expect(await gate.remainingSlots(company.id)).toBe(8);
    });

    it("setConfig persists to the companies row and applies live", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Persist Co");
      const updated = await gate.setConfig(company.id, { maxInFlight: 4 });
      expect(updated).toEqual({ enabled: true, maxInFlight: 4 });
      expect(await gate.remainingSlots(company.id)).toBe(4);
      const [row] = await db.select().from(companies).where(eq(companies.id, company.id));
      expect(row.scheduling).toEqual({ enabled: true, maxInFlight: 4 });
      // Live change: tightening the cap shrinks the remainder immediately.
      const tightened = await gate.setConfig(company.id, { maxInFlight: 2 });
      expect(tightened).toEqual({ enabled: true, maxInFlight: 2 });
      expect(await gate.remainingSlots(company.id)).toBe(2);
    });

    it("a fresh gate instance after a restart reads the persisted config", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Restart Co");
      await gate.setConfig(company.id, { maxInFlight: 4 });
      const restarted = new SchedulingGate();
      restarted.bind(db);
      const config = await restarted.configFor(company.id);
      expect(config).toEqual({ enabled: true, maxInFlight: 4 });
    });

    it("enabled false disables the cap (today's behavior)", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Disabled Co");
      await gate.setConfig(company.id, { enabled: false });
      expect(await gate.remainingSlots(company.id)).toBe(Number.POSITIVE_INFINITY);
      const reserved = gate.reserve(company.id, 99);
      expect(reserved).toBe(99);
      expect(gate.reservedCount(company.id)).toBe(0);
    });

    it("validateSchedulingConfigPatch accepts and rejects per the contract", () => {
      expect(validateSchedulingConfigPatch({ maxInFlight: 4 })).toEqual({ maxInFlight: 4 });
      expect(validateSchedulingConfigPatch({ enabled: false })).toEqual({ enabled: false });
      expect(validateSchedulingConfigPatch({ enabled: true, maxInFlight: 256 })).toEqual({ enabled: true, maxInFlight: 256 });
      for (const body of [
        { maxInFlight: 0 },
        { maxInFlight: 257 },
        { maxInFlight: 1.5 },
        { maxInFlight: "8" },
        { enabled: "yes" },
        { slots: 3 },
        null,
        [1],
      ]) {
        expect(() => validateSchedulingConfigPatch(body)).toThrow();
      }
    });
  });

  describe("restart seeding", () => {
    it("seeds inFlight from heartbeat_runs WHERE status='running' and avoids double-counting re-adoptions", async () => {
      gate.bind(db);
      const companyA = await fixtureCompany("Seed A");
      const companyB = await fixtureCompany("Seed B");
      const agentA = await fixtureAgent(companyA.id, "Seed agent A");
      const agentB = await fixtureAgent(companyB.id, "Seed agent B");
      const runsA: Run[] = [];
      const runsB: Run[] = [];
      for (const [i, company, list] of [
        [0, companyA, runsA],
        [1, companyA, runsA],
        [2, companyB, runsB],
      ] as const) {
        const agent = i === 2 ? agentB : agentA;
        const { run } = await fixtureQueuedRun(company.id, agent, `Seed ${i}`);
        await db.update(heartbeatRuns).set({ status: "running", startedAt: new Date() }).where(eq(heartbeatRuns.id, run.id));
        list.push((await db.select().from(heartbeatRuns).where(eq(heartbeatRuns.id, run.id)))[0]);
      }
      const { run: queuedRun } = await fixtureQueuedRun(companyA.id, agentA, "Still queued");
      expect(queuedRun.status).toBe("queued");

      const seeded = await gate.seedFromDatabase();
      expect(seeded).toBe(3);
      expect(gate.inFlightCount(companyA.id)).toBe(2);
      expect(gate.inFlightCount(companyB.id)).toBe(1);
      // Seeding is idempotent.
      expect(await gate.seedFromDatabase()).toBe(3);
      expect(gate.inFlightCount(companyA.id)).toBe(2);

      // Restart recovery re-executes a re-adopted run: the mirror fires, but
      // the seeded run is already counted, so no double count.
      gate.onExecutionStarted(runsA[0].id, companyA.id);
      expect(gate.inFlightCount(companyA.id)).toBe(2);
      gate.onExecutionFinished(runsA[0].id);
      expect(gate.inFlightCount(companyA.id)).toBe(1);
    });

    it("a restart with in-flight runs keeps the cap exact for the next claim", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Seed cap");
      const agent = await fixtureAgent(company.id, "Seed cap agent");
      await gate.setConfig(company.id, { maxInFlight: 4 });
      const { run } = await fixtureQueuedRun(company.id, agent, "Running one");
      await db.update(heartbeatRuns).set({ status: "running", startedAt: new Date() }).where(eq(heartbeatRuns.id, run.id));

      const restarted = new SchedulingGate();
      restarted.bind(db);
      await restarted.seedFromDatabase();
      expect(restarted.inFlightCount(company.id)).toBe(1);
      expect(await restarted.remainingSlots(company.id)).toBe(3);
      const reserved = restarted.reserve(company.id, 2);
      expect(reserved).toBe(2);
      expect(restarted.inFlightCount(company.id) + restarted.reservedCount(company.id)).toBeLessThanOrEqual(4);
    });
  });

  describe("plugin consultation (fail-open)", () => {
    function params(overrides: Partial<SchedulingGateConsultParams> = {}): SchedulingGateConsultParams {
      return {
        companyId: "company-plugin",
        agentId: "agent-plugin",
        run: {
          runId: "run-plugin",
          agentId: "agent-plugin",
          companyId: "company-plugin",
          status: "queued",
          issueId: null,
          invocationSource: "assignment",
          triggerDetail: null,
          createdAt: new Date().toISOString(),
        },
        hostSlotsRemaining: 3,
        ...overrides,
      };
    }

    it("returns the host count when no plugin implements the method", async () => {
      gate.bind(db, fakeManager([
        fakeWorker("nope", { supportedMethods: ["onEvent"] }),
        fakeWorker("none", { supportedMethods: [] }),
      ]));
      const result = await gate.consultPlugins(params());
      expect(result).toEqual({ grantedSlots: 3, defer: false, plugin: null });
    });

    it("fails open when the worker is not running", async () => {
      gate.bind(db, fakeManager([fakeWorker("paused", { status: "stopped", supportedMethods: ["schedulingGate"] })]));
      const result = await gate.consultPlugins(params());
      expect(result).toEqual({ grantedSlots: 3, defer: false, plugin: null });
    });

    it("fails open when the RPC rejects (timeout surfaces as a rejection)", async () => {
      const call = vi.fn(async () => {
        throw new Error("call timed out after 2000ms");
      });
      gate.bind(db, fakeManager([fakeWorker("slow", { supportedMethods: ["schedulingGate"], call })]));
      const result = await gate.consultPlugins(params());
      expect(result).toEqual({ grantedSlots: 3, defer: false, plugin: null });
      expect(call).toHaveBeenCalledWith("schedulingGate", expect.anything(), SCHEDULING_GATE_CONSULT_TIMEOUT_MS);
    });

    it("fails open on a malformed result shape", async () => {
      gate.bind(db, fakeManager([
        fakeWorker("stringy", { supportedMethods: ["schedulingGate"], call: vi.fn(async () => "nope") as PluginWorkerHandle["call"] }),
        fakeWorker("nullish", { supportedMethods: ["schedulingGate"], call: vi.fn(async () => null) as PluginWorkerHandle["call"] }),
      ]));
      const result = await gate.consultPlugins(params());
      expect(result).toEqual({ grantedSlots: 3, defer: false, plugin: null });
    });

    it("clamps a plugin slots value above the host remainder and honors a lower one", async () => {
      const call = vi.fn(async (_method: string, p: SchedulingGateConsultParams) => ({
        slots: p.hostSlotsRemaining > 5 ? 10 : 1,
      }));
      const worker = fakeWorker("clamp", { supportedMethods: ["schedulingGate"], call });
      gate.bind(db, fakeManager([worker]));
      const high = await gate.consultPlugins(params({ hostSlotsRemaining: 6 }));
      expect(high.grantedSlots).toBe(6);
      const low = await gate.consultPlugins(params({ hostSlotsRemaining: 2 }));
      expect(low.grantedSlots).toBe(1);
      expect(low.plugin?.pluginId).toBe("clamp");
    });

    it("propagates defer and records the plugin descriptor", async () => {
      gate.bind(db, fakeManager([
        fakeWorker("hold", {
          supportedMethods: ["schedulingGate"],
          call: vi.fn(async () => ({ defer: true, reason: "holding last slot for unblocking agent" })),
        }),
      ]));
      const result = await gate.consultPlugins(params());
      expect(result.defer).toBe(true);
      expect(result.plugin?.pluginId).toBe("hold");
      expect(result.plugin?.reason).toBe("holding last slot for unblocking agent");
    });

    it("takes the tightest slots when several plugins respond", async () => {
      gate.bind(db, fakeManager([
        fakeWorker("loose", { supportedMethods: ["schedulingGate"], call: vi.fn(async () => ({ slots: 3 })) }),
        fakeWorker("tight", { supportedMethods: ["schedulingGate"], call: vi.fn(async () => ({ slots: 1, reason: "routines last" })) }),
        fakeWorker("broken", { supportedMethods: ["schedulingGate"], call: vi.fn(async () => { throw new Error("worker down"); }) }),
      ]));
      const result = await gate.consultPlugins(params());
      expect(result.grantedSlots).toBe(1);
      expect(result.defer).toBe(false);
      expect(result.plugin?.pluginId).toBe("tight");
    });

    it("consultPlugins throws are caught by the caller: the gate never throws on a bad manager", async () => {
      const manager = { listWorkers: () => { throw new Error("manager exploded"); } } as unknown as PluginWorkerManager;
      gate.bind(db, manager);
      // consultPlugins itself lets a listWorkers throw propagate; the queue
      // catches it. Verify the documented shape of that failure here.
      await expect(gate.consultPlugins(params())).rejects.toThrow("manager exploded");
    });
  });

  describe("queue claim path integration", () => {
    it("caps concurrent claims across 6 agents at maxInFlight (burst)", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Burst Co");
      await gate.setConfig(company.id, { maxInFlight: 4 });
      const startedRunIds: string[] = [];
      const callbacks = gateCallbacks(db, {
        executeRun: vi.fn(async (runId: string) => {
          const [run] = await db.select().from(heartbeatRuns).where(eq(heartbeatRuns.id, runId));
          if (!run) return;
          // Mirror the real executeRun start-site hook.
          gate.onExecutionStarted(runId, run.companyId);
          startedRunIds.push(runId);
        }),
      });
      const queue = createHeartbeatQueue(db, callbacks);
      const fixtures = [];
      for (let i = 0; i < 6; i += 1) {
        const agent = await fixtureAgent(company.id, `Burst agent ${i}`);
        fixtures.push(await fixtureQueuedRun(company.id, agent, `Burst task ${i}`));
      }
      // Fire every agent's claim pass at once: per-agent start locks do not
      // serialize across agents, so only the gate's reservation can hold the
      // cap under this burst.
       const claimed = await Promise.all(
         fixtures.map(({ agent }) => queue.startNextQueuedRunForAgent(agent.id)),
       );
       await drainExecutions(callbacks);
       const totalClaimed = claimed.reduce((sum, runs) => sum + runs.length, 0);
       expect(totalClaimed).toBeLessThanOrEqual(4);
       expect(totalClaimed).toBeGreaterThan(0);
       expect(startedRunIds.length).toBe(totalClaimed);
      expect(gate.inFlightCount(company.id) + gate.reservedCount(company.id)).toBeLessThanOrEqual(4);
      expect(gate.inFlightCount(company.id)).toBe(totalClaimed);
      // The unclaimed runs stay queued for the next pass.
      const [queuedCount] = await db
        .select({ count: sql<number>`count(*)` })
        .from(heartbeatRuns)
        .where(and(eq(heartbeatRuns.companyId, company.id), eq(heartbeatRuns.status, "queued")));
      expect(Number(queuedCount?.count ?? 0)).toBe(6 - totalClaimed);
      // A follow-up pass after the first batch "finishes" drains the rest.
      for (const runId of startedRunIds) {
        await db.update(heartbeatRuns).set({ status: "succeeded", finishedAt: new Date() }).where(eq(heartbeatRuns.id, runId));
        gate.onExecutionFinished(runId);
      }
      expect(gate.inFlightCount(company.id)).toBe(0);
       const second = await Promise.all(
         fixtures.map(({ agent }) => queue.startNextQueuedRunForAgent(agent.id)),
       );
       await drainExecutions(callbacks);
       const secondClaimed = second.reduce((sum, runs) => sum + runs.length, 0);
      expect(secondClaimed).toBeGreaterThan(0);
      expect(gate.inFlightCount(company.id) + gate.reservedCount(company.id)).toBeLessThanOrEqual(4);
    });

    it("holds the cap under staggered re-entry (reservations survive the pass finally until the start site)", async () => {
      // Regression for ANT-903's major finding: the pass finally used to
      // release every claimed run's reservation as a microtask BEFORE the
      // fire-and-forget executions reached their start sites (executeRun
      // suspends at getSchedulingSuppression and further awaits first), so a
      // second pass landing in that gap double-booked the freed slots.
      gate.bind(db);
      const company = await fixtureCompany("Stagger Co");
      await gate.setConfig(company.id, { maxInFlight: 4 });
      const agentA = await fixtureAgent(company.id, "Stagger agent A");
      const agentB = await fixtureAgent(company.id, "Stagger agent B");
      const agentC = await fixtureAgent(company.id, "Stagger agent C");
      // Two seeded in-flight runs under a third agent: agents A and B keep
      // their full per-agent capacity (2) while the company starts with 2 of
      // 4 slots used.
      for (let i = 0; i < 2; i += 1) {
        const { run } = await fixtureQueuedRun(company.id, agentC, `Seeded ${i}`);
        await db.update(heartbeatRuns).set({ status: "running", startedAt: new Date() }).where(eq(heartbeatRuns.id, run.id));
        gate.onExecutionStarted(run.id, company.id);
      }
      expect(gate.inFlightCount(company.id)).toBe(2);
      const fixtureA1 = await fixtureQueuedRun(company.id, agentA, "A queued 1");
      const fixtureA2 = await fixtureQueuedRun(company.id, agentA, "A queued 2");
      await fixtureQueuedRun(company.id, agentB, "B queued 1");
      await fixtureQueuedRun(company.id, agentB, "B queued 2");

      const samples: number[] = [];
      const invariant = () => {
        const total = gate.inFlightCount(company.id) + gate.reservedCount(company.id);
        samples.push(total);
        expect(total).toBeLessThanOrEqual(4);
      };
      const callbacks = gateCallbacks(db, {
        executeRun: vi.fn(async (runId: string) => {
          const [run] = await db.select().from(heartbeatRuns).where(eq(heartbeatRuns.id, runId));
          if (!run) return;
          // Delayed start site: mirrors the real pre-start awaits that land
          // after the claim pass's finally has already run.
          await new Promise((resolve) => setTimeout(resolve, 40));
          gate.onExecutionStarted(runId, run.companyId);
          invariant();
        }),
      });
      const queue = createHeartbeatQueue(db, callbacks);

      // Pass A claims both remaining slots with delayed starts.
      const claimedA = await queue.startNextQueuedRunForAgent(agentA.id);
      expect(claimedA.length).toBe(2);
      invariant();
      // The fix: A's claimed runs keep their reservations after the pass
      // finally (which releases only the unclaimed remainder, here 0). The
      // old code released them here, leaving 2 phantom-free slots.
      expect(gate.reservedCount(company.id)).toBe(2);

      // Pass B lands in the staggered gap: A's runs are claimed but not yet
      // started. The held reservations leave no room, so B claims nothing.
      const claimedB = await queue.startNextQueuedRunForAgent(agentB.id);
      expect(claimedB.length).toBe(0);
      invariant();
      expect(gate.reservedCount(company.id)).toBe(2);

      // A's runs reach their delayed start sites (conversion, not release).
      await drainExecutions(callbacks);
      invariant();
      expect(gate.inFlightCount(company.id)).toBe(4);
      expect(gate.reservedCount(company.id)).toBe(0);

      // A's runs finish; the slots free for B's queued runs on the next pass.
      for (const runId of [fixtureA1.run.id, fixtureA2.run.id]) {
        await db.update(heartbeatRuns).set({ status: "succeeded", finishedAt: new Date() }).where(eq(heartbeatRuns.id, runId));
        gate.onExecutionFinished(runId);
      }
      invariant();
      expect(gate.inFlightCount(company.id)).toBe(2);
      const claimedBSecond = await queue.startNextQueuedRunForAgent(agentB.id);
      expect(claimedBSecond.length).toBe(2);
      invariant();
      await drainExecutions(callbacks);
      invariant();
      // Sampled at the start sites and on every pass exit: the invariant
      // never broke, including the staggered gap the old code overshot to 6.
      expect(samples.length).toBeGreaterThan(0);
      expect(Math.max(...samples)).toBeLessThanOrEqual(4);
    });

    it("enabled false reproduces today's per-agent-only claim behavior", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Legacy Co");
      await gate.setConfig(company.id, { enabled: false });
      const startedRunIds: string[] = [];
      const callbacks = gateCallbacks(db, {
        executeRun: vi.fn(async (runId: string) => {
          const [run] = await db.select().from(heartbeatRuns).where(eq(heartbeatRuns.id, runId));
          if (!run) return;
          gate.onExecutionStarted(runId, run.companyId);
          startedRunIds.push(runId);
        }),
      });
      const queue = createHeartbeatQueue(db, callbacks);
      const fixtures = [];
      for (let i = 0; i < 4; i += 1) {
        const agent = await fixtureAgent(company.id, `Legacy agent ${i}`);
        fixtures.push(await fixtureQueuedRun(company.id, agent, `Legacy task ${i}`));
      }
       const claimed = await Promise.all(
         fixtures.map(({ agent }) => queue.startNextQueuedRunForAgent(agent.id)),
       );
       await drainExecutions(callbacks);
       // 4 agents x maxConcurrentRuns 2, each with one queued run: all four
       // claim with no global cap in play.
       expect(claimed.reduce((sum, runs) => sum + runs.length, 0)).toBe(4);
       expect(startedRunIds.length).toBe(4);
      expect(gate.reservedCount(company.id)).toBe(0);
    });

    it("a deferring plugin holds the claim: runs stay queued and the reservation is settled", async () => {
      gate.bind(db);
      const company = await fixtureCompany("Defer Co");
      await gate.setConfig(company.id, { maxInFlight: 4 });
      const callback = vi.fn(async () => ({ defer: true, reason: "routine last" }));
      gate.bind(db, fakeManager([
        fakeWorker("policy", { supportedMethods: ["schedulingGate"], call: callback }),
      ]));
      const callbacks = gateCallbacks(db, {
        executeRun: vi.fn(async () => undefined),
      });
      const queue = createHeartbeatQueue(db, callbacks);
      const agent = await fixtureAgent(company.id, "Defer agent");
      const { run } = await fixtureQueuedRun(company.id, agent, "Deferred task");
      const claimed = await queue.startNextQueuedRunForAgent(agent.id);
      expect(claimed).toEqual([]);
      expect(callback).toHaveBeenCalled();
      const [row] = await db.select().from(heartbeatRuns).where(eq(heartbeatRuns.id, run.id));
      expect(row.status).toBe("queued");
      expect(gate.reservedCount(company.id)).toBe(0);
      expect(gate.inFlightCount(company.id)).toBe(0);
    });
  });
});
