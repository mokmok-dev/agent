import { describe, expect, test } from "bun:test";
import {
  deltaOf,
  piArgs,
  piConfigFrom,
  piEnv,
  PiAdapter,
  promptText,
  settleOf,
  stopReasonFor,
  type PiConfig,
  type PiProcess,
} from "./pi-adapter.ts";

/** A pi process the test drives, standing in for the child the entry spawns. */
class FakePi implements PiProcess {
  readonly commands: Record<string, unknown>[] = [];
  readonly frames: unknown[] = [];
  stopped = false;
  #line: ((line: string) => void) | undefined;

  send(command: Record<string, unknown>): void {
    this.commands.push(command);
  }

  onLine(listener: (line: string) => void): void {
    this.#line = listener;
  }

  stop(): void {
    this.stopped = true;
  }

  /** Write one line as pi would, which the adapter routes to its session. */
  emit(line: string): void {
    this.#line?.(line);
  }
}

const config: PiConfig = {
  provider: "ollama-cloud",
  model: "deepseek-v4.1-flash",
  agentDir: "/tmp/pi-agent",
  turnTimeoutMs: 1000,
};

const adapterOver = (times: number[] = [], turnTimeoutMs = 1000) => {
  const processes: FakePi[] = [];
  const written: Record<string, unknown>[] = [];
  let id = 0;
  const adapter = new PiAdapter({
    config: { ...config, turnTimeoutMs },
    ports: {
      write: (frame) => written.push(frame as Record<string, unknown>),
      startPi: () => {
        const pi = new FakePi();
        processes.push(pi);
        return pi;
      },
      commandId: () => {
        id += 1;
        return `p-${id}`;
      },
      timer: (ms, fire) => {
        times.push(ms);
        let live = true;
        const handle = setTimeout(() => {
          if (live) fire();
        }, ms);
        return () => {
          live = false;
          clearTimeout(handle);
        };
      },
    },
  });
  return { adapter, processes, written, times };
};

/** Answer a request through the adapter and return the frame it wrote. */
const ask = (adapter: PiAdapter, id: number, method: string, params: unknown = {}): void => {
  adapter.handle(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
};

const lastWritten = (written: Record<string, unknown>[]): Record<string, unknown> => {
  const frame = written.at(-1);
  if (frame === undefined) throw new Error("the adapter wrote nothing");
  return frame;
};

describe("pi settings", () => {
  test("the environment supplies the provider, model, agent directory and timeout", () => {
    expect(
      piConfigFrom({
        PI_PROVIDER: "ollama-cloud",
        PI_MODEL: "some-model",
        PI_CODING_AGENT_DIR: "/tmp/agent",
        PI_TURN_TIMEOUT_MS: "5000",
      }),
    ).toEqual({
      provider: "ollama-cloud",
      model: "some-model",
      agentDir: "/tmp/agent",
      turnTimeoutMs: 5000,
    });
  });

  test("an unstated setting falls back to the shipped default", () => {
    expect(piConfigFrom({})).toEqual({
      provider: "ollama-cloud",
      model: "deepseek-v4.1-flash",
      agentDir: undefined,
      turnTimeoutMs: 240_000,
    });
  });

  test("each stated setting is read from its own variable", () => {
    // A setting read from the wrong variable would show up as a change in
    // another field, so each is checked alone.
    expect(piConfigFrom({ PI_PROVIDER: "p" }).provider).toBe("p");
    expect(piConfigFrom({ PI_MODEL: "m" }).model).toBe("m");
    expect(piConfigFrom({ PI_CODING_AGENT_DIR: "d" }).agentDir).toBe("d");
    expect(piConfigFrom({ PI_TURN_TIMEOUT_MS: "7" }).turnTimeoutMs).toBe(7);
  });

  test("pi is started in RPC mode with the configured provider and model", () => {
    expect(piArgs({ ...config, provider: "ollama-cloud", model: "deepseek-v4.1-flash" })).toEqual([
      "--mode",
      "rpc",
      "--no-session",
      "--provider",
      "ollama-cloud",
      "--model",
      "deepseek-v4.1-flash",
    ]);
  });

  test("the child environment states pi's agent directory when one is configured", () => {
    expect(piEnv({ HOME: "/home/someone" }, { ...config, agentDir: "/tmp/pi-agent" })).toEqual({
      HOME: "/home/someone",
      PI_CODING_AGENT_DIR: "/tmp/pi-agent",
    });
  });

  test("the child environment is untouched when no agent directory is configured", () => {
    const base = { HOME: "/home/someone" };
    expect(piEnv(base, { ...config, agentDir: undefined })).toBe(base);
  });
});

describe("prompt text", () => {
  test("text blocks are joined, in order, one per line", () => {
    expect(
      promptText([
        { type: "text", text: "first" },
        { type: "text", text: "second" },
      ]),
    ).toBe("first\nsecond");
  });

  test("non-text blocks and unnamed blocks are dropped", () => {
    expect(
      promptText([{ type: "image", data: "??" }, { type: "text", text: "kept" }, { nope: true }]),
    ).toBe("kept");
  });

  test("a prompt that is not a list has no text", () => {
    expect(promptText("not a list")).toBe("");
    expect(promptText(undefined)).toBe("");
  });

  test("a block with no type is not sent to pi", () => {
    expect(promptText([{ text: "untyped" }])).toBe("");
    expect(promptText([{ type: "text", text: "typed" }])).toBe("typed");
  });

  test("a text field that is not a string is not sent to pi", () => {
    expect(promptText([{ type: "text", text: 42 }])).toBe("");
  });
});

describe("pi event frames", () => {
  test("a text delta is the forwarded text", () => {
    expect(
      deltaOf({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "hello" },
      }),
    ).toBe("hello");
  });

  test("another event type inside a message update is not a delta", () => {
    expect(
      deltaOf({
        type: "message_update",
        assistantMessageEvent: { type: "thinking_delta", delta: "hmm" },
      }),
    ).toBeUndefined();
  });

  test("another frame type is not a delta", () => {
    expect(deltaOf({ type: "agent_settled" })).toBeUndefined();
  });

  test("a delta that is not a string is not forwarded", () => {
    expect(
      deltaOf({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: 7 } }),
    ).toBeUndefined();
  });

  test("a message update with no event at all is not a delta", () => {
    expect(deltaOf({ type: "message_update" })).toBeUndefined();
  });

  test("an event that is not a message update carries no delta, however it is shaped", () => {
    // Only `message_update` frames are read for text; a frame of another type
    // that happens to hold a delta field is still not forwarded.
    expect(
      deltaOf({ type: "tool_result", assistantMessageEvent: { type: "text_delta", delta: "x" } }),
    ).toBeUndefined();
  });

  test("agent_settled ends the turn with end_turn", () => {
    expect(settleOf({ type: "agent_settled" })).toBe("end_turn");
    expect(settleOf({ type: "message_update" })).toBeUndefined();
  });

  test("a turn with text reports the reason pi gave, and one without reports an error", () => {
    expect(stopReasonFor(true, "end_turn")).toBe("end_turn");
    expect(stopReasonFor(false, "end_turn")).toBe("error");
  });
});

describe("ACP requests", () => {
  test("initialize answers with protocol version 1 and the prompt capabilities", () => {
    const { adapter, written } = adapterOver();
    ask(adapter, 1, "initialize");
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 1,
      result: {
        protocolVersion: 1,
        agentCapabilities: { promptCapabilities: { embeddedContext: true, image: false } },
        agentInfo: { name: "pi-adapter", version: "0" },
      },
    });
  });

  test("session/new starts one pi process and names the session", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new", { cwd: "/work" });
    expect(processes.length).toBe(1);
    // pi's model is fixed by its command line, so the one it can run is the one
    // it was started with, and the control plane can see that.
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 1,
      result: {
        sessionId: "pi-1",
        models: {
          currentModelId: "deepseek-v4.1-flash",
          availableModels: [{ modelId: "deepseek-v4.1-flash", name: "deepseek-v4.1-flash" }],
        },
      },
    });
    expect(adapter.sessionIds()).toEqual(["pi-1"]);
  });

  test("a second session gets its own process and its own id", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/new");
    expect(processes.length).toBe(2);
    expect(lastWritten(written)).toMatchObject({
      jsonrpc: "2.0",
      id: 2,
      result: { sessionId: "pi-2" },
    });
    expect(adapter.sessionIds()).toEqual(["pi-1", "pi-2"]);
  });

  test("a prompt becomes a pi prompt command naming the turn's text", () => {
    const { adapter, processes } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "say hi" }],
    });
    expect(processes[0]!.commands).toEqual([{ id: "p-1", type: "prompt", message: "say hi" }]);
  });

  test("a prompt for an unknown session is refused by name", () => {
    const { adapter, written } = adapterOver();
    ask(adapter, 1, "session/prompt", {
      sessionId: "pi-9",
      prompt: [{ type: "text", text: "hi" }],
    });
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 1,
      error: { code: -32602, message: "no pi session pi-9" },
    });
  });

  test("a prompt with no session id names the empty id it looked for", () => {
    const { adapter, written } = adapterOver();
    ask(adapter, 1, "session/prompt", { prompt: [{ type: "text", text: "hi" }] });
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 1,
      error: { code: -32602, message: "no pi session " },
    });
  });

  test("an unimplemented method is refused with its name", () => {
    const { adapter, written } = adapterOver();
    ask(adapter, 1, "session/load");
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 1,
      error: { code: -32601, message: "pi adapter does not implement session/load" },
    });
  });

  test("an unparseable request is refused as a parse error", () => {
    const { adapter, written } = adapterOver();
    adapter.handle("{not json");
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: null,
      error: { code: -32700, message: "pi adapter could not parse a frame" },
    });
  });

  test("a request with no params answers without reading them", () => {
    const { adapter, written } = adapterOver();
    adapter.handle(JSON.stringify({ jsonrpc: "2.0", id: 3, method: "initialize" }));
    expect(lastWritten(written)).toMatchObject({ id: 3, result: { protocolVersion: 1 } });
  });
});

describe("a turn over pi", () => {
  test("each delta becomes an agent_message_chunk, and settling answers with end_turn", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "say hi" }],
    });
    processes[0]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "hel" },
      }),
    );
    processes[0]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "lo" },
      }),
    );
    expect(written.slice(-2, -1)[0]).toEqual({
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        sessionId: "pi-1",
        update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hel" } },
      },
    });
    processes[0]!.emit(JSON.stringify({ type: "agent_settled" }));
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 2,
      result: { stopReason: "end_turn" },
    });
  });

  test("a settled turn that produced no text answers with error", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "say hi" }],
    });
    processes[0]!.emit(JSON.stringify({ type: "agent_settled" }));
    expect(lastWritten(written)).toEqual({
      jsonrpc: "2.0",
      id: 2,
      result: { stopReason: "error" },
    });
  });

  test("a turn that never settles times out as cancelled, once", async () => {
    const { adapter, processes, written } = adapterOver([], 20);
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "say hi" }],
    });
    await Bun.sleep(20);
    // A late settle after the timeout must not answer the same request twice.
    processes[0]!.emit(JSON.stringify({ type: "agent_settled" }));
    expect(written.filter((frame) => frame["id"] === 2)).toEqual([
      { jsonrpc: "2.0", id: 2, result: { stopReason: "cancelled" } },
    ]);
  });

  test("the configured turn timeout is the one applied", async () => {
    const times: number[] = [];
    const { adapter, processes } = adapterOver(times);
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "hi" }],
    });
    expect(times).toEqual([1000]);
    // The timer is cancelled once the turn settles, so nothing extra fires.
    processes[0]!.emit(JSON.stringify({ type: "agent_settled" }));
  });

  test("a settle cancels the timeout, so a slow turn is not answered twice", async () => {
    const { adapter, processes, written } = adapterOver([], 10);
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "hi" }],
    });
    processes[0]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "done" },
      }),
    );
    processes[0]!.emit(JSON.stringify({ type: "agent_settled" }));
    // Longer than the turn timeout, so a live timer would answer a second time.
    await Bun.sleep(40);
    expect(written.filter((frame) => frame["id"] === 2)).toEqual([
      { jsonrpc: "2.0", id: 2, result: { stopReason: "end_turn" } },
    ]);
  });

  test("a frame that is neither a delta nor a settle leaves the turn running", async () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "hi" }],
    });
    processes[0]!.emit(JSON.stringify({ type: "tool_start" }));
    expect(written.filter((frame) => frame["id"] === 2)).toEqual([]);
  });

  test("pi's startup chatter is dropped, and so is a frame after the turn ends", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    processes[0]!.emit("not json at all");
    processes[0]!.emit(JSON.stringify({ type: "ready" }));
    expect(written.length).toBe(1);
  });

  test("a delta for no turn in flight is dropped", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    processes[0]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "stray" },
      }),
    );
    expect(written.length).toBe(1);
  });

  test("a line that is not JSON is dropped even while a turn is in flight", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/prompt", {
      sessionId: "pi-1",
      prompt: [{ type: "text", text: "hi" }],
    });
    // Pi writes startup chatter that is not a frame; it must not disturb a turn.
    processes[0]!.emit("pi is thinking about it");
    processes[0]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "still here" },
      }),
    );
    expect(written.at(-1)).toEqual({
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        sessionId: "pi-1",
        update: {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "still here" },
        },
      },
    });
  });

  test("one session's lines do not reach another session", () => {
    const { adapter, processes, written } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/new");
    ask(adapter, 3, "session/prompt", {
      sessionId: "pi-2",
      prompt: [{ type: "text", text: "to the second" }],
    });
    processes[1]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "second only" },
      }),
    );
    processes[0]!.emit(
      JSON.stringify({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "first too" },
      }),
    );
    expect(written.filter((frame) => frame["method"] === "session/update")).toEqual([
      {
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          sessionId: "pi-2",
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: "second only" },
          },
        },
      },
    ]);
  });
});

describe("cancelling and closing", () => {
  test("a cancel stops that session's process and forgets the session", () => {
    const { adapter, processes } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/cancel", { sessionId: "pi-1" });
    expect(processes[0]!.stopped).toBe(true);
    expect(adapter.sessionIds()).toEqual([]);
  });

  test("a cancel for a session that is gone is harmless", () => {
    const { adapter, written } = adapterOver();
    ask(adapter, 1, "session/cancel", { sessionId: "pi-1" });
    expect(written).toEqual([]);
  });

  test("a cancel with no session id stops no live session", () => {
    const { adapter, processes } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/cancel");
    expect(processes[0]!.stopped).toBe(false);
    expect(adapter.sessionIds()).toEqual(["pi-1"]);
  });

  test("closing stops every session and forgets them", () => {
    const { adapter, processes } = adapterOver();
    ask(adapter, 1, "session/new");
    ask(adapter, 2, "session/new");
    adapter.closeAll();
    expect(processes.map((pi) => pi.stopped)).toEqual([true, true]);
    expect(adapter.sessionIds()).toEqual([]);
  });
});
