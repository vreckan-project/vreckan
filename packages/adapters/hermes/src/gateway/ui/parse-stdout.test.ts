import { describe, expect, it } from "vitest";
import { parseHermesGatewayStdoutLine } from "./parse-stdout.js";

const TS = "2026-07-08T12:00:00.000Z";

function eventLine(eventName: string, data: unknown): string {
  return `[hermes-gateway:event] run=run-123 event=${eventName} data=${JSON.stringify(data)}`;
}

describe("parseHermesGatewayStdoutLine — reasoning.available payload extraction", () => {
  // This is the assertion that FAILS on the old hardcoded-placeholder code
  // and PASSES once the real reasoning text is extracted from `data`.
  it("uses the real reasoning text from data.text instead of the hardcoded placeholder", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("reasoning.available", { text: "Considering three approaches to the cache invalidation bug." }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      kind: "thinking",
      ts: TS,
      text: "Considering three approaches to the cache invalidation bug.",
    });
    expect(result[0]).not.toHaveProperty("text", "Hermes reasoning available");
  });

  it("uses the real reasoning text from data.summary", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("reasoning.available", { summary: "Weighing tradeoffs between two refactor strategies." }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      kind: "thinking",
      ts: TS,
      text: "Weighing tradeoffs between two refactor strategies.",
    });
  });

  it("recurses one level into a nested data.data record to find the reasoning text", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("reasoning.available", { data: { text: "Nested reasoning payload from gateway wrapper." } }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      kind: "thinking",
      ts: TS,
      text: "Nested reasoning payload from gateway wrapper.",
    });
  });

  it("recurses one level into a nested data.payload record to find the reasoning text", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("reasoning.available", { payload: { reasoning: "Nested via payload wrapper instead of data." } }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      kind: "thinking",
      ts: TS,
      text: "Nested via payload wrapper instead of data.",
    });
  });

  it("falls back to the placeholder when data has no recognizable text field (preserves bare-signal behavior)", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("reasoning.available", { unrelatedField: 42 }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ kind: "thinking", ts: TS, text: "Hermes reasoning available" });
  });

  it("falls back to the placeholder when data is entirely absent/unparseable", () => {
    const result = parseHermesGatewayStdoutLine(
      "[hermes-gateway:event] run=run-123 event=reasoning.available data=not-json",
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ kind: "thinking", ts: TS, text: "Hermes reasoning available" });
  });
});

describe("parseHermesGatewayStdoutLine — regression guards for unrelated handlers", () => {
  it("still yields an assistant delta part for message.delta", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("message.delta", { delta: "Hello there" }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ kind: "assistant", ts: TS, text: "Hello there", delta: true });
  });

  it("still yields a stdout part for a plain non-event line", () => {
    const result = parseHermesGatewayStdoutLine("just a plain line of output", TS);
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ kind: "stdout", ts: TS, text: "just a plain line of output" });
  });
});

describe("parseHermesGatewayStdoutLine — tool event cards", () => {
  it("renders tool.started as a tool_call entry with the tool name and preview", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("tool.started", {
        event: "tool.started",
        run_id: "run-123",
        timestamp: 1,
        tool: "shell",
        preview: 'bash -c "cargo test"',
        tool_call_id: "call_1",
      }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      kind: "tool_call",
      ts: TS,
      name: "shell",
      input: { preview: 'bash -c "cargo test"' },
      toolUseId: "call_1",
    });
  });

  it("pairs tool.completed with tool.started via tool_call_id and carries duration/error into the result", () => {
    const started = parseHermesGatewayStdoutLine(
      eventLine("tool.started", { event: "tool.started", run_id: "run-123", timestamp: 1, tool: "shell", preview: "ls -la", tool_call_id: "call_2" }),
      TS,
    );
    const completed = parseHermesGatewayStdoutLine(
      eventLine("tool.completed", {
        event: "tool.completed",
        run_id: "run-123",
        timestamp: 2,
        tool: "shell",
        duration: 0.12,
        error: false,
        preview: "total 42\nCargo.toml",
        tool_call_id: "call_2",
      }),
      TS,
    );
    if (started[0].kind !== "tool_call") throw new Error("expected tool_call entry");
    expect(started[0].toolUseId).toBe("call_2");
    expect(completed).toHaveLength(1);
    expect(completed[0]).toEqual({
      kind: "tool_result",
      ts: TS,
      toolUseId: "call_2",
      toolName: "shell",
      content: "total 42\nCargo.toml  0.12s",
      isError: false,
    });
  });

  it("marks the tool_result as an error when the gateway reports error=true", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("tool.completed", {
        event: "tool.completed",
        run_id: "run-123",
        timestamp: 3,
        tool: "read_file",
        duration: 0.01,
        error: true,
        preview: "ENOENT: no such file",
        tool_call_id: "call_3",
      }),
      TS,
    );
    expect(result[0]).toMatchObject({ kind: "tool_result", isError: true, toolUseId: "call_3" });
  });

  it("pairs id-less tool.started with the next id-less tool.completed via the FIFO queue", () => {
    const started = parseHermesGatewayStdoutLine(
      eventLine("tool.started", { event: "tool.started", run_id: "run-123", timestamp: 4, tool: "web_search", preview: "vllm qwen38 reasoning" }),
      TS,
    );
    if (started[0].kind !== "tool_call") throw new Error("expected tool_call entry");
    const startedId = started[0].toolUseId;
    expect(startedId).toBeTruthy();
    const completed = parseHermesGatewayStdoutLine(
      eventLine("tool.completed", { event: "tool.completed", run_id: "run-123", timestamp: 5, tool: "web_search", duration: 1.4, error: false, preview: "12 results" }),
      TS,
    );
    expect(completed[0]).toMatchObject({ kind: "tool_result", toolUseId: startedId, content: "12 results  1.4s", isError: false });
  });

  it("renders an id-less completed with no pending started as a standalone tool_result", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("tool.completed", { event: "tool.completed", run_id: "run-123", timestamp: 6, tool: "shell", duration: 2, error: false }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0].kind).toBe("tool_result");
    expect(result[0]).toMatchObject({ toolName: "shell", content: "[shell] 2s", isError: false });
    if (result[0].kind === "tool_result") expect(result[0].toolUseId).toBeTruthy();
  });
});

describe("parseHermesGatewayStdoutLine — message.interim", () => {
  it("renders interim commentary as an assistant entry", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("message.interim", { event: "message.interim", run_id: "run-123", timestamp: 1, text: "Let me check the logs first.", already_streamed: false }),
      TS,
    );
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ kind: "assistant", ts: TS, text: "Let me check the logs first." });
  });

  it("drops interim events with empty text", () => {
    const result = parseHermesGatewayStdoutLine(
      eventLine("message.interim", { event: "message.interim", run_id: "run-123", timestamp: 1, text: "   " }),
      TS,
    );
    expect(result).toHaveLength(0);
  });
});
