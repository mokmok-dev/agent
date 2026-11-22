import { describe, expect, test } from "bun:test";
import { AgentRecorder, until } from "./recorder.ts";
import { AcpBridge, errorMessage, type GatewayEvent } from "./bridge.ts";
import { ProtocolError } from "@agent/protocol";
import { encodeEvent, EventLog, parseEdgeFrame } from "./edge.ts";
import { decodeMessage } from "@agent/protocol";

const bridge = (options: { mcpServers?: "array" | "record" } = {}) => {
  const agent = new AgentRecorder();
  const events: GatewayEvent[] = [];
  const bridge = new AcpBridge({
    channel: agent,
    emit: (event) => events.push(event),
    ...(options.mcpServers === undefined ? {} : { mcpServers: options.mcpServers }),
  });
  agent.inbound = (text) => bridge.receive(text);
  return { agent, events, bridge };
};

const tick = () => Promise.resolve();

describe("open", () => {
  test("initializes, opens a session, and reports the bound id", async () => {
    const { agent, events, bridge: acp } = bridge();
    const opened = acp.open("sess-1", "/work");
    expect(agent.last()).toMatchObject({ kind: "request", method: "initialize", id: 1 });
    agent.answer(1, { protocolVersion: 1 });
    await until(() => agent.outbound().length === 2);
    expect(agent.last()).toMatchObject({ kind: "request", method: "session/new", id: 2 });
    agent.answer(2, { sessionId: "acp-9" });
    expect(await opened).toBe("acp-9");
    // An agent that states no models still reports its capability, so the
    // control plane can tell "this agent names none" from "nothing arrived".
    expect(events).toEqual([
      { type: "session.bound", sessionId: "sess-1", acpSessionId: "acp-9" },
      {
        type: "session.capability",
        sessionId: "sess-1",
        capability: { availableModels: [], statesModels: false },
      },
    ]);
  });

  test("the capability names the agent, its model list, and its current model", async () => {
    const { agent, events, bridge: acp } = bridge();
    void acp.open("sess-1", "/work");
    agent.answer(1, { protocolVersion: 1, agentInfo: { name: "OpenCode", version: "2.3.0" } });
    await until(() => agent.outbound().length === 2);
    agent.answer(2, {
      sessionId: "acp-9",
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model",
          type: "select",
          currentValue: "ollama-cloud/deepseek-v4.1-flash",
          options: [
            { value: "ollama-cloud/deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash" },
            { value: "opencode/tidepool-alpha-free", name: "Tidepool" },
          ],
        },
        { id: "mode", name: "Session Mode", category: "mode", currentValue: "build" },
      ],
    });
    await until(() => events.length === 2);
    expect(events.at(-1)).toEqual({
      type: "session.capability",
      sessionId: "sess-1",
      capability: {
        agent: { name: "OpenCode", version: "2.3.0" },
        currentModelId: "ollama-cloud/deepseek-v4.1-flash",
        availableModels: ["ollama-cloud/deepseek-v4.1-flash", "opencode/tidepool-alpha-free"],
        statesModels: true,
      },
    });
  });

  test("a model list stated in `models` is read when there is no settings entry", async () => {
    const { agent, events, bridge: acp } = bridge();
    void acp.open("sess-1", "/work");
    agent.answer(1, { protocolVersion: 1 });
    await until(() => agent.outbound().length === 2);
    agent.answer(2, {
      sessionId: "acp-9",
      models: {
        currentModelId: "deepseek-v4.1-flash",
        availableModels: [
          { modelId: "grok-4.7", name: "Grok 4.7" },
          { modelId: "deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash" },
        ],
      },
    });
    await until(() => events.length === 2);
    expect(events.at(-1)).toMatchObject({
      capability: {
        currentModelId: "deepseek-v4.1-flash",
        availableModels: ["grok-4.7", "deepseek-v4.1-flash"],
        statesModels: true,
      },
    });
  });

  test("sends the client info so the agent can name the gateway", async () => {
    const { agent, bridge: acp } = bridge();
    void acp.open("s", "/work");
    expect(agent.sent[0]).toMatchObject({
      params: { clientInfo: { name: "agent-gateway", version: "0" } },
    });
  });
});

describe("run", () => {
  const open = async () => {
    const context = bridge();
    const opened = context.bridge.open("sess-1", "/work");
    context.agent.answer(1, { protocolVersion: 1 });
    await until(() => context.agent.outbound().length === 2);
    context.agent.answer(2, { sessionId: "acp-9" });
    await opened;
    context.events.length = 0;
    return context;
  };

  test("reports started, the updates, and completion in order", async () => {
    const { agent, events, bridge: acp } = await open();
    const running = acp.run("sess-1", "acp-9", "turn-1", "do it");
    expect(agent.last()).toMatchObject({
      method: "session/prompt",
      params: { sessionId: "acp-9" },
    });
    agent.update("acp-9", {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "ok" },
    });
    agent.answer(3, { stopReason: "end_turn" });
    await running;
    expect(events.map((event) => event.type)).toEqual([
      "turn.started",
      "turn.update",
      "turn.completed",
    ]);
    expect(events[2]).toMatchObject({ stopReason: "end_turn" });
  });

  test("sends the prompt as one text block", async () => {
    const { agent, bridge: acp } = await open();
    const running = acp.run("sess-1", "acp-9", "turn-1", "hello");
    const last = agent.last();
    expect(last).toMatchObject({
      params: { sessionId: "acp-9", prompt: [{ type: "text", text: "hello" }] },
    });
    agent.answer(3, { stopReason: "end_turn" });
    await running;
  });

  test("a rejected prompt reports a failure with its code", async () => {
    const { agent, events, bridge: acp } = await open();
    const running = acp.run("sess-1", "acp-9", "turn-1", "hello");
    agent.fail(3, -32000, "auth required");
    await running;
    expect(events.at(-1)).toEqual({
      type: "turn.failed",
      turnId: "turn-1",
      sessionId: "sess-1",
      error: "-32000: auth required",
    });
  });

  test("a response that is not a prompt result fails the turn with its message", async () => {
    const { agent, events, bridge: acp } = await open();
    const running = acp.run("sess-1", "acp-9", "turn-1", "hello");
    agent.answer(3, { stopReason: "because" });
    await running;
    expect(events.at(-1)).toMatchObject({
      type: "turn.failed",
      error: "-32603: value has the wrong shape",
    });
  });

  test("updates after the turn ends are not attributed to it", async () => {
    const { agent, events, bridge: acp } = await open();
    const running = acp.run("sess-1", "acp-9", "turn-1", "hello");
    agent.answer(3, { stopReason: "end_turn" });
    await running;
    const before = events.length;
    agent.update("acp-9", {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "late" },
    });
    expect(events.length).toBe(before);
  });

  test("a second turn takes over the attribution", async () => {
    const { agent, events, bridge: acp } = await open();
    const first = acp.run("sess-1", "acp-9", "turn-1", "one");
    agent.answer(3, { stopReason: "end_turn" });
    await first;
    const second = acp.run("sess-1", "acp-9", "turn-2", "two");
    agent.update("acp-9", {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "x" },
    });
    agent.answer(4, { stopReason: "end_turn" });
    await second;
    expect(events.filter((event) => event.type === "turn.update")).toHaveLength(1);
    expect(events.find((event) => event.type === "turn.update")).toMatchObject({
      turnId: "turn-2",
    });
  });
});

describe("cancel", () => {
  test("sends session/cancel as a notification", async () => {
    const { agent, bridge: acp } = bridge();
    acp.cancel("acp-9");
    expect(agent.last()).toEqual({
      kind: "notification",
      method: "session/cancel",
      params: { sessionId: "acp-9" },
    });
  });
});

describe("permission", () => {
  const ask = async () => {
    const context = await (async () => {
      const { agent, events, bridge: inner } = bridge();
      const opened = inner.open("sess-1", "/work");
      agent.answer(1, { protocolVersion: 1 });
      await until(() => agent.outbound().length === 2);
      agent.answer(2, { sessionId: "acp-9" });
      await opened;
      events.length = 0;
      return { agent, events, bridge: inner };
    })();
    const running = context.bridge.run("sess-1", "acp-9", "turn-1", "delete it");
    context.agent.askPermission(50, {
      sessionId: "acp-9",
      toolCall: { toolCallId: "call-1", title: "rm" },
      options: [
        { optionId: "allow", name: "Allow", kind: "allow_once" },
        { optionId: "deny", name: "Deny", kind: "reject_once" },
      ],
    });
    await tick();
    return { ...context, running };
  };

  test("a permission request is reported and waits for the answer", async () => {
    const { events, bridge: acp } = await ask();
    expect(events).toContainEqual({
      type: "permission.asked",
      turnId: "turn-1",
      toolCallId: "call-1",
      options: ["allow", "deny"],
    });
    expect(acp.answer("call-1", "allow")).toBe(true);
    expect(acp.answer("call-1", "allow")).toBe(false);
  });

  test("the chosen option is what the agent receives", async () => {
    const { agent, bridge: acp, running } = await ask();
    acp.answer("call-1", "allow");
    await until(() => agent.sent.at(-1)?.kind === "result", 40);
    expect(agent.sent.at(-1)).toEqual({
      kind: "result",
      id: 50,
      result: { outcome: "selected", optionId: "allow" },
    });
    agent.answer(3, { stopReason: "end_turn" });
    await running;
  });

  test("a declined decision answers the agent with cancelled", async () => {
    const { agent, bridge: acp, running } = await ask();
    acp.answer("call-1", undefined);
    await until(() => agent.sent.at(-1)?.kind === "result", 40);
    expect(agent.sent.at(-1)).toEqual({ kind: "result", id: 50, result: { outcome: "cancelled" } });
    agent.answer(3, { stopReason: "cancelled" });
    await running;
  });

  test("answering an unknown request changes nothing", async () => {
    const { bridge: acp } = await ask();
    expect(acp.answer("unknown", "allow")).toBe(false);
  });
});

describe("event log", () => {
  const event = (turnId: string): GatewayEvent => ({
    type: "turn.started",
    turnId,
    sessionId: "s",
  });

  test("retains events in order and reports what is unacknowledged", () => {
    const log = new EventLog(10);
    log.append(event("1"));
    log.append(event("2"));
    expect(log.unacknowledged().map((entry) => entry.type)).toEqual([
      "turn.started",
      "turn.started",
    ]);
    expect(log.pending).toBe(2);
    log.acknowledge();
    expect(log.unacknowledged()).toEqual([]);
    expect(log.pending).toBe(0);
    expect(log.retained()).toHaveLength(2);
  });

  test("drops the oldest event past its limit", () => {
    const log = new EventLog(2);
    log.append(event("1"));
    log.append(event("2"));
    log.append(event("3"));
    expect(log.retained()).toHaveLength(2);
    expect(log.unacknowledged()).toEqual([event("2"), event("3")]);
  });

  test("a drop after an acknowledgement does not go negative", () => {
    const log = new EventLog(2);
    log.append(event("1"));
    log.acknowledge();
    log.append(event("2"));
    log.append(event("3"));
    expect(log.pending).toBeGreaterThanOrEqual(0);
  });

  test("an empty log has nothing to send", () => {
    expect(new EventLog(5).unacknowledged()).toEqual([]);
    expect(new EventLog(5).pending).toBe(0);
  });
});

describe("edge frames", () => {
  test("parses each command", () => {
    expect(
      parseEdgeFrame(JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "ping" } })),
    ).toEqual({
      type: "ping",
    });
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "x",
          params: { type: "cancel", acpSessionId: "a" },
        }),
      ),
    ).toEqual({ type: "cancel", acpSessionId: "a" });
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "x",
          params: { type: "permission", toolCallId: "c", optionId: null },
        }),
      ),
    ).toEqual({ type: "permission", toolCallId: "c", optionId: null });
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "x",
          params: { type: "permission", toolCallId: "c" },
        }),
      ),
    ).toEqual({ type: "permission", toolCallId: "c", optionId: null });
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "x",
          params: { type: "prompt", sessionId: "s", acpSessionId: "a", turnId: "t", text: "hi" },
        }),
      ),
    ).toEqual({ type: "prompt", sessionId: "s", acpSessionId: "a", turnId: "t", text: "hi" });
  });

  test("a malformed or unknown frame is undefined", () => {
    for (const text of [
      "nope",
      JSON.stringify({ jsonrpc: "2.0", id: 1, result: 1 }),
      JSON.stringify({ jsonrpc: "2.0", method: "x" }),
      JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "unknown" } }),
      JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "cancel" } }),
      JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "prompt", sessionId: "s" } }),
      JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "permission" } }),
    ]) {
      expect(parseEdgeFrame(text)).toBeUndefined();
    }
  });

  test("an event encodes as a gateway notification", () => {
    const text = encodeEvent({ type: "session.bound", sessionId: "s", acpSessionId: "a" });
    const decoded = decodeMessage(text);
    if (decoded.kind === "rejected") throw new Error(decoded.message);
    expect(decoded.message).toEqual({
      kind: "notification",
      method: "gateway/event",
      params: { type: "session.bound", sessionId: "s", acpSessionId: "a" },
    });
  });
});

describe("bridge state", () => {
  test("pending counts the requests still open", async () => {
    const { bridge: acp } = bridge();
    void acp.open("s", "/work");
    expect(acp.pending).toBe(1);
  });
});

describe("edges the survivors pointed at", () => {
  test("an update before any turn starts is dropped", () => {
    const { agent, events, bridge: acp } = bridge();
    agent.update("acp-9", {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "x" },
    });
    void acp;
    expect(events).toEqual([]);
  });

  test("an update during a turn but before a session is bound is dropped", () => {
    const { agent, events, bridge: acp } = bridge();
    void acp.open("s", "/w");
    agent.answer(1, { protocolVersion: 1 });
    agent.answer(2, { sessionId: "acp-9" });
    events.length = 0;
    agent.update("acp-9", {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "x" },
    });
    expect(events).toEqual([]);
  });

  test("a permission request outside a turn reports an empty turn id", async () => {
    const { agent, events } = bridge();
    agent.askPermission(60, {
      sessionId: "acp-9",
      toolCall: { toolCallId: "call-9", title: "x" },
      options: [{ optionId: "ok", name: "OK", kind: "allow_once" }],
    });
    await tick();
    expect(events).toContainEqual({
      type: "permission.asked",
      turnId: "",
      toolCallId: "call-9",
      options: ["ok"],
    });
  });
});

describe("edge frame kinds", () => {
  test("a result frame is not a command", () => {
    expect(parseEdgeFrame(JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }))).toBeUndefined();
  });

  test("an error frame is not a command", () => {
    expect(
      parseEdgeFrame(JSON.stringify({ jsonrpc: "2.0", id: 1, error: { code: 1, message: "m" } })),
    ).toBeUndefined();
  });
});

describe("more attribution cases", () => {
  test("an update with a session but no turn is dropped", async () => {
    const { agent, events, bridge: acp } = bridge();
    const opened = acp.open("sess-1", "/work");
    agent.answer(1, { protocolVersion: 1 });
    await until(() => agent.outbound().length === 2);
    agent.answer(2, { sessionId: "acp-9" });
    await opened;
    events.length = 0;
    agent.update("acp-9", { sessionUpdate: "usage_update", used: 1, size: 2 });
    expect(events).toEqual([]);
  });

  test("a non-Error failure reports its string form", async () => {
    const { agent, events, bridge: acp } = bridge();
    const running = acp.run("s", "a", "t", "x");
    agent.deliver(JSON.stringify({ jsonrpc: "2.0", id: 1, error: { code: 7, message: "seven" } }));
    await running;
    expect(events.at(-1)).toMatchObject({ type: "turn.failed", error: "7: seven" });
  });

  test("a session/new frame carries the working directory and the agent's MCP shape", async () => {
    const asArray = bridge();
    void asArray.bridge.open("s", "/the/work/dir");
    asArray.agent.answer(1, { protocolVersion: 1 });
    await until(() => asArray.agent.outbound().length === 2);
    // `grok agent stdio` requires an array and refuses a record.
    expect(asArray.agent.last()).toEqual({
      kind: "request",
      id: 2,
      method: "session/new",
      params: { cwd: "/the/work/dir", mcpServers: [] },
    });

    // `opencode2 acp` requires a record and refuses an array.
    const asRecord = bridge({ mcpServers: "record" });
    void asRecord.bridge.open("s", "/the/work/dir");
    asRecord.agent.answer(1, { protocolVersion: 1 });
    await until(() => asRecord.agent.outbound().length === 2);
    expect(asRecord.agent.last()).toEqual({
      kind: "request",
      id: 2,
      method: "session/new",
      params: { cwd: "/the/work/dir", mcpServers: {} },
    });
  });
});

describe("error messages", () => {
  test("a JSON-RPC error keeps its code", () => {
    expect(errorMessage(new ProtocolError(-32000, "auth required"))).toBe("-32000: auth required");
  });

  test("a plain error reports its message", () => {
    expect(errorMessage(new Error("boom"))).toBe("boom");
  });

  test("a thrown non-error reports its string form", () => {
    expect(errorMessage("plain")).toBe("plain");
    expect(errorMessage(42)).toBe("42");
    expect(errorMessage(undefined)).toBe("undefined");
  });

  test("an error that puts the real cause in its data keeps that cause", () => {
    // grok answers a turn with `-32603: Internal error` and the actual reason
    // nested in `data`; reporting only the code would hide why it stopped.
    expect(
      errorMessage(
        new ProtocolError(-32603, "Internal error", {
          message: "API error (status 402 Payment Required): usage balance exhausted",
          http_status: 402,
        }),
      ),
    ).toBe(
      "-32603: Internal error (API error (status 402 Payment Required): usage balance exhausted)",
    );
  });

  test("a data field that is a plain string is used as the detail", () => {
    expect(errorMessage(new ProtocolError(-32603, "Internal error", "no such model"))).toBe(
      "-32603: Internal error (no such model)",
    );
  });

  test("data that names no reason adds nothing, so the message stays clean", () => {
    const clean = "-32603: Internal error";
    expect(errorMessage(new ProtocolError(-32603, "Internal error"))).toBe(clean);
    expect(errorMessage(new ProtocolError(-32603, "Internal error", {}))).toBe(clean);
    expect(errorMessage(new ProtocolError(-32603, "Internal error", ""))).toBe(clean);
    expect(errorMessage(new ProtocolError(-32603, "Internal error", 402))).toBe(clean);
    expect(errorMessage(new ProtocolError(-32603, "Internal error", { message: "" }))).toBe(clean);
    expect(errorMessage(new ProtocolError(-32603, "Internal error", { message: 42 }))).toBe(clean);
  });

  test("a detail field other than message is read when that is what the agent names", () => {
    expect(errorMessage(new ProtocolError(-1, "x", { detail: "the detail" }))).toBe(
      "-1: x (the detail)",
    );
    expect(errorMessage(new ProtocolError(-1, "x", { reason: "the reason" }))).toBe(
      "-1: x (the reason)",
    );
  });
});

describe("a frame the edge cannot read", () => {
  test("a body that is not JSON reads as no command", () => {
    expect(parseEdgeFrame("not json")).toBeUndefined();
  });

  test("a JSON-RPC message with params that are not a frame reads as no command", () => {
    expect(
      parseEdgeFrame(JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "nope" } })),
    ).toBeUndefined();
  });

  test("a well-formed frame reads as its command", () => {
    expect(
      parseEdgeFrame(JSON.stringify({ jsonrpc: "2.0", method: "x", params: { type: "ping" } })),
    ).toEqual({ type: "ping" });
  });
});

describe("the text a turn produced", () => {
  const openTurn = async (updates: unknown[]) => {
    const context = bridge();
    const opened = context.bridge.open("sess-1", "/work");
    context.agent.answer(1, { protocolVersion: 1 });
    await until(() => context.agent.outbound().length === 2);
    context.agent.answer(2, { sessionId: "acp-9" });
    await opened;
    context.events.length = 0;
    const running = context.bridge.run("sess-1", "acp-9", "turn-1", "do it");
    for (const update of updates) {
      context.agent.update("acp-9", update as never);
    }
    context.agent.answer(3, { stopReason: "end_turn" });
    await running;
    return context;
  };

  test("the completed event carries the agent's message text", async () => {
    const { events } = await openTurn([
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "the answer" } },
    ]);
    expect(events.at(-1)).toEqual({
      type: "turn.completed",
      turnId: "turn-1",
      sessionId: "sess-1",
      stopReason: "end_turn",
      text: "the answer",
    });
  });

  test("several chunks are concatenated in order", async () => {
    const { events } = await openTurn([
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "one " } },
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "two" } },
    ]);
    expect((events.at(-1) as { text?: string }).text).toBe("one two");
  });

  test("a turn with no message chunks carries an empty text, never a missing one", async () => {
    const { events } = await openTurn([
      { sessionUpdate: "tool_call", toolCallId: "t", status: "completed" },
    ]);
    expect(events.at(-1)).toMatchObject({ type: "turn.completed", text: "" });
  });

  test("a non-text content block does not become the reply", async () => {
    const { events } = await openTurn([
      { sessionUpdate: "agent_message_chunk", content: { type: "thinking", thinking: "hidden" } },
    ]);
    expect((events.at(-1) as { text?: string }).text).toBe("");
  });
});

describe("selecting the routed model", () => {
  /** Open a session and answer `session/new` with the capability it states. */
  const openWith = async (
    session: Record<string, unknown>,
    initialized: Record<string, unknown> = { protocolVersion: 1 },
  ) => {
    const harness = bridge();
    void harness.bridge.open("sess-1", "/work");
    harness.agent.answer(1, initialized);
    await until(() => harness.agent.outbound().length === 2);
    harness.agent.answer(2, { sessionId: "acp-9", ...session });
    await until(() => harness.events.length === 2);
    return harness;
  };

  test("a model the agent lists is selected by the value it lists", async () => {
    const { agent, bridge: acp } = await openWith({
      models: {
        currentModelId: "default-model",
        availableModels: [
          { modelId: "grok-4.6", name: "Grok 4.6" },
          { modelId: "default-model", name: "Default" },
        ],
      },
    });
    const selecting = acp.selectModel("acp-9", { provider: "xai", modelId: "grok-4.6" });
    await until(() => agent.outbound().length === 3);
    expect(agent.last()).toEqual({
      kind: "request",
      id: 3,
      method: "session/set_config_option",
      params: { sessionId: "acp-9", configId: "model", value: "grok-4.6" },
    });
    agent.answer(3, { configOptions: [] });
    expect(await selecting).toBe(true);
  });

  test("a model the agent lists with a provider prefix is selected by that value", async () => {
    const { agent, bridge: acp } = await openWith({
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model",
          currentValue: "opencode/other",
          options: [{ value: "ollama-cloud/deepseek-v4.1-flash", name: "DeepSeek" }],
        },
      ],
    });
    const selecting = acp.selectModel("acp-9", {
      provider: "ollama-cloud",
      modelId: "deepseek-v4.1-flash",
    });
    await until(() => agent.outbound().length === 3);
    expect(agent.last()).toMatchObject({
      method: "session/set_config_option",
      params: { value: "ollama-cloud/deepseek-v4.1-flash" },
    });
    agent.answer(3, {});
    expect(await selecting).toBe(true);
  });

  test("a model the agent does not list is not selected at all", async () => {
    const { agent, bridge: acp } = await openWith({
      models: { currentModelId: "only", availableModels: [{ modelId: "only", name: "Only" }] },
    });
    expect(await acp.selectModel("acp-9", { provider: "xai", modelId: "grok-4.6" })).toBe(false);
    // Nothing was sent, so the agent keeps the model it chose.
    expect(agent.outbound()).toHaveLength(2);
  });

  test("an agent that states no models has nothing to select", async () => {
    const { agent, bridge: acp } = await openWith({});
    expect(await acp.selectModel("acp-9", { provider: "xai", modelId: "grok-4.6" })).toBe(false);
    expect(agent.outbound()).toHaveLength(2);
  });

  test("an agent that refuses the setting leaves the turn to run on its own model", async () => {
    const { agent, bridge: acp } = await openWith({
      models: { currentModelId: "a", availableModels: [{ modelId: "a", name: "A" }] },
    });
    const selecting = acp.selectModel("acp-9", { provider: "p", modelId: "a" });
    await until(() => agent.outbound().length === 3);
    agent.fail(3, -32601, "method not found");
    expect(await selecting).toBe(false);
  });
});

describe("the model a prompt carries", () => {
  test("a frame with a model reaches the bridge as that model", () => {
    const frame = parseEdgeFrame(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "gateway/command",
        params: {
          type: "prompt",
          sessionId: "s",
          acpSessionId: "a",
          turnId: "t",
          text: "x",
          model: { provider: "xai", modelId: "grok-4.6" },
        },
      }),
    );
    expect(frame).toEqual({
      type: "prompt",
      sessionId: "s",
      acpSessionId: "a",
      turnId: "t",
      text: "x",
      model: { provider: "xai", modelId: "grok-4.6" },
    });
  });

  test("a frame with no model reaches the bridge without one, which means the agent chooses", () => {
    const frame = parseEdgeFrame(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "gateway/command",
        params: { type: "prompt", sessionId: "s", acpSessionId: "a", turnId: "t", text: "x" },
      }),
    );
    expect(frame).not.toHaveProperty("model");
  });

  test("a model that names no provider is dropped, and the turn runs on the agent's own choice", () => {
    // A half-read model cannot select anything, so the frame keeps the turn and
    // loses only the unusable model, rather than dropping the prompt.
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/command",
          params: {
            type: "prompt",
            sessionId: "s",
            acpSessionId: "a",
            turnId: "t",
            text: "x",
            model: { modelId: "grok-4.6" },
          },
        }),
      ),
    ).toEqual({
      type: "prompt",
      sessionId: "s",
      acpSessionId: "a",
      turnId: "t",
      text: "x",
    });
  });
});

describe("selecting before a session is open", () => {
  test("an agent with no reported capability selects nothing rather than guessing", async () => {
    // A gateway that has not opened a session has nothing to select against, so
    // the call must report that rather than send a request it cannot justify.
    const harness = bridge();
    expect(await harness.bridge.selectModel("acp-9", { provider: "xai", modelId: "m" })).toBe(
      false,
    );
    expect(harness.agent.outbound()).toHaveLength(0);
  });
});

describe("an error whose data is not an object", () => {
  test("a null data adds nothing, which is the same as no data at all", () => {
    expect(errorMessage(new ProtocolError(-1, "x", null))).toBe("-1: x");
  });
});

describe("a capability the bridge kept", () => {
  test("an agent that listed models but named none now still selects nothing", async () => {
    // The empty-list case is reached only when the agent's own list is empty,
    // which is different from having no capability at all.
    const harness = bridge();
    void harness.bridge.open("sess-1", "/work");
    harness.agent.answer(1, { protocolVersion: 1 });
    await until(() => harness.agent.outbound().length === 2);
    harness.agent.answer(2, {
      sessionId: "acp-9",
      models: { currentModelId: "a", availableModels: [{ modelId: "a", name: "A" }] },
    });
    await until(() => harness.events.length === 2);
    expect(await harness.bridge.selectModel("acp-9", { provider: "p", modelId: "other" })).toBe(
      false,
    );
  });
});

describe("a prompt frame whose model fails to decode", () => {
  test("a model that is not an object is dropped rather than failing the frame", () => {
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/command",
          params: {
            type: "prompt",
            sessionId: "s",
            acpSessionId: "a",
            turnId: "t",
            text: "x",
            model: "grok-4.6",
          },
        }),
      ),
    ).toEqual({ type: "prompt", sessionId: "s", acpSessionId: "a", turnId: "t", text: "x" });
  });

  test("a prompt frame of another kind is refused, which is what the kind guard decides", () => {
    // The decoder answers "rejected" for a frame whose `type` is not prompt,
    // so the guard turns it into a dropped command rather than a prompt.
    expect(
      parseEdgeFrame(
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/command",
          params: { type: "not-prompt", sessionId: "s" },
        }),
      ),
    ).toBeUndefined();
  });
});

describe("the model list a selection reads", () => {
  test("an agent that listed no models and one that never reported both select nothing", async () => {
    // The two empty cases arrive by different routes: one capability with an
    // empty list, and no capability at all. Neither may send a request.
    const listingNone = bridge();
    void listingNone.bridge.open("sess-1", "/work");
    listingNone.agent.answer(1, { protocolVersion: 1 });
    await until(() => listingNone.agent.outbound().length === 2);
    listingNone.agent.answer(2, {
      sessionId: "acp-9",
      models: { currentModelId: "a", availableModels: [] },
    });
    await until(() => listingNone.events.length === 2);
    expect(await listingNone.bridge.selectModel("acp-9", { provider: "p", modelId: "a" })).toBe(
      false,
    );
    expect(listingNone.agent.outbound()).toHaveLength(2);

    const unreported = bridge();
    expect(await unreported.bridge.selectModel("acp-9", { provider: "p", modelId: "a" })).toBe(
      false,
    );
    expect(unreported.agent.outbound()).toHaveLength(0);
  });
});
