import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import { AcpClient } from "./client.ts";
import {
  ClientMethod,
  AgentMethod,
  PROTOCOL_VERSION,
  STOP_REASONS,
  decodeContentBlock,
  decodeCreateTerminalParams,
  decodeInitializeResult,
  decodeNewSessionResult,
  decodePromptResult,
  decodeReadTextFileParams,
  decodeRequestPermissionParams,
  decodeSessionUpdate,
  decodeTerminalOutputResult,
  decodeTerminalRefParams,
  isStopReason,
} from "./types.ts";
import { decodeMessage, ProtocolError, RpcErrorCode, type RpcMessage } from "../jsonrpc.ts";
import type { Decoded } from "../decode.ts";

class Recorder {
  readonly sent: RpcMessage[] = [];

  send(text: string): void {
    const decoded = decodeMessage(text);
    if (decoded.kind === "read") this.sent.push(decoded.message);
  }

  last(): RpcMessage {
    const message = this.sent.at(-1);
    if (!message) throw new Error("nothing was sent");
    return message;
  }
}

/** The refusal a decoder returned, as the protocol error the client raises for it. */
function rejection<T>(decoded: Decoded<T>): ProtocolError {
  if (decoded.kind !== "rejected") throw new Error("expected a refusal");
  expect(decoded.kind).toBe("rejected");
  return new ProtocolError(RpcErrorCode.InvalidRequest, decoded.message);
}

function value<T>(decoded: Decoded<T>): T {
  if (decoded.kind !== "decoded") throw new Error("expected a value");
  return decoded.value;
}

describe("initialize", () => {
  test("sends ACP v1 with text-only capabilities", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      clientInfo: { name: "control-plane", version: "1.2.3" },
    });
    const result = client.initialize(client.defaultInitializeParams());
    expect(recorder.last()).toEqual({
      kind: "request",
      id: 1,
      method: AgentMethod.Initialize,
      params: {
        protocolVersion: PROTOCOL_VERSION,
        clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
        clientInfo: { name: "control-plane", version: "1.2.3" },
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        result: { protocolVersion: 1, agentCapabilities: { loadSession: true } },
      }),
    );
    expect(await result).toEqual({ protocolVersion: 1, agentCapabilities: { loadSession: true } });
  });

  test("omits clientInfo when none is configured", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    expect(client.defaultInitializeParams()).toEqual({
      protocolVersion: PROTOCOL_VERSION,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
    });
  });

  test("keeps the capabilities and version the caller passes", () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      clientCapabilities: { fs: { readTextFile: true }, terminal: true },
      protocolVersion: 2,
    });
    expect(client.defaultInitializeParams()).toEqual({
      protocolVersion: 2,
      clientCapabilities: { fs: { readTextFile: true }, terminal: true },
    });
  });
});

describe("request correlation", () => {
  test("ids increase by one and each response settles its own request", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    const first = client.newSession({ cwd: "/work" });
    const second = client.prompt("s1", [{ type: "text", text: "hi" }]);
    expect(recorder.sent.map((message) => message.kind === "request" && message.id)).toEqual([
      1, 2,
    ]);
    expect(client.pendingCount).toBe(2);

    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 2, result: { stopReason: "end_turn" } }));
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { sessionId: "s1" } }));

    expect(await first).toEqual({ sessionId: "s1" });
    expect(await second).toEqual({ stopReason: "end_turn" });
    expect(client.pendingCount).toBe(0);
  });

  test("an error reply rejects that request with its code, message, and data", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    const pending = client.newSession({ cwd: "/work" });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        error: { code: RpcErrorCode.AuthRequired, message: "log in first", data: { url: "x" } },
      }),
    );
    const error = (await pending.catch((reason: unknown) => reason)) as ProtocolError;
    expect(error).toBeInstanceOf(ProtocolError);
    expect(error.code).toBe(RpcErrorCode.AuthRequired);
    expect(error.message).toBe("log in first");
    expect(error.data).toEqual({ url: "x" });
  });

  test("a response for an unknown id is ignored", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 99, result: { sessionId: "s" } }));
    expect(client.pendingCount).toBe(0);
  });

  test("a second response for the same id does not settle twice", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    const pending = client.newSession({ cwd: "/work" });
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { sessionId: "a" } }));
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { sessionId: "b" } }));
    expect(await pending).toEqual({ sessionId: "a" });
  });

  test("a malformed frame is dropped without throwing", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    expect(() => client.receive("not json")).not.toThrow();
    expect(() => client.receive('{"jsonrpc":"1.0","method":"x"}')).not.toThrow();
    expect(client.pendingCount).toBe(0);
  });

  test("cancel is a notification, not a request", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.cancel("s1");
    expect(recorder.last()).toEqual({
      kind: "notification",
      method: AgentMethod.Cancel,
      params: { sessionId: "s1" },
    });
    expect(client.pendingCount).toBe(0);
  });

  test("setMode sends the mode request", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    const pending = client.setMode("s1", "plan");
    expect(recorder.last()).toMatchObject({
      kind: "request",
      method: AgentMethod.SetMode,
      params: { sessionId: "s1", modeId: "plan" },
    });
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: null }));
    expect(await pending).toBeNull();
  });
});

describe("session updates", () => {
  test("each update kind reaches the handler parsed", () => {
    const recorder = new Recorder();
    const seen: unknown[] = [];
    const client = new AcpClient({
      channel: recorder,
      handlers: { onSessionUpdate: (u) => seen.push(u) },
    });

    const frames = [
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hello" } },
      { sessionUpdate: "user_message_chunk", content: { type: "text", text: "hi" } },
      { sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "thinking" } },
      {
        sessionUpdate: "tool_call",
        toolCall: { toolCallId: "t1", title: "read", kind: "read", status: "pending" },
      },
      { sessionUpdate: "tool_call_update", toolCall: { toolCallId: "t1", status: "completed" } },
      {
        sessionUpdate: "plan",
        entries: [
          { content: "one", status: "pending" },
          { content: "two", status: "in_progress" },
        ],
      },
      { sessionUpdate: "current_mode_update", currentModeId: "ask" },
      {
        sessionUpdate: "available_commands_update",
        availableCommands: [{ name: "review", description: "Run review" }],
      },
      {
        sessionUpdate: "session_info_update",
        title: "Fix the build",
        updatedAt: "2026-01-01T00:00:00Z",
      },
      { sessionUpdate: "usage_update", used: 10, size: 100 },
    ];
    for (const [index, update] of frames.entries()) {
      client.receive(
        JSON.stringify({
          jsonrpc: "2.0",
          method: ClientMethod.SessionUpdate,
          params: { sessionId: "s1", update },
        }),
      );
      expect(seen[index]).toEqual(update);
    }
  });

  test("a non-update notification is ignored", () => {
    const recorder = new Recorder();
    const seen: unknown[] = [];
    const client = new AcpClient({
      channel: recorder,
      handlers: { onSessionUpdate: (u) => seen.push(u) },
    });
    client.receive(JSON.stringify({ jsonrpc: "2.0", method: "something/else", params: { a: 1 } }));
    expect(seen).toEqual([]);
  });

  test("an update with no handler is dropped", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    expect(() =>
      client.receive(
        JSON.stringify({ jsonrpc: "2.0", method: ClientMethod.SessionUpdate, params: {} }),
      ),
    ).not.toThrow();
  });
});

describe("agent requests", () => {
  test("request_permission is answered through the handler", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        requestPermission: async (params) => {
          expect(params.sessionId).toBe("s1");
          expect(params.toolCall.toolCallId).toBe("t1");
          expect(params.options.map((option) => option.optionId)).toEqual(["yes", "no"]);
          return { outcome: "selected", optionId: "yes" };
        },
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 5,
        method: ClientMethod.RequestPermission,
        params: {
          sessionId: "s1",
          toolCall: { toolCallId: "t1", title: "Run tests" },
          options: [
            { optionId: "yes", name: "Allow", kind: "allow_once" },
            { optionId: "no", name: "Deny", kind: "reject_once" },
          ],
        },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "result",
      id: 5,
      result: { outcome: "selected", optionId: "yes" },
    });
  });

  test("an unhandled agent request is refused with MethodNotFound", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 7,
        method: ClientMethod.ReadTextFile,
        params: { sessionId: "s", path: "/x" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 7,
      error: {
        code: RpcErrorCode.MethodNotFound,
        message: "this client does not handle fs/read_text_file",
      },
    });
  });

  test("an unknown method is MethodNotFound", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 8, method: "weird/method", params: {} }));
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 8,
      error: {
        code: RpcErrorCode.MethodNotFound,
        message: "this client does not handle weird/method",
      },
    });
  });

  test("a handler that throws becomes an internal error reply", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        readTextFile: () => {
          throw new Error("disk on fire");
        },
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 9,
        method: ClientMethod.ReadTextFile,
        params: { sessionId: "s", path: "/x" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 9,
      error: { code: RpcErrorCode.InternalError, message: "disk on fire" },
    });
  });

  test("a handler that throws a ProtocolError keeps its code", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        readTextFile: () =>
          Promise.reject(new ProtocolError(RpcErrorCode.ResourceNotFound, "no such file")),
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 10,
        method: ClientMethod.ReadTextFile,
        params: { sessionId: "s", path: "/x" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 10,
      error: { code: RpcErrorCode.ResourceNotFound, message: "no such file" },
    });
  });

  test("a handler that throws a non-Error value reports its string form", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: { killTerminal: () => Promise.reject("nope") },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 11,
        method: ClientMethod.KillTerminal,
        params: { sessionId: "s", terminalId: "t" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 11,
      error: { code: RpcErrorCode.InternalError, message: "nope" },
    });
  });

  test("every method refuses when its handler is missing", async () => {
    const calls: Array<[string, unknown]> = [
      [
        ClientMethod.RequestPermission,
        { sessionId: "s", toolCall: { toolCallId: "t" }, options: [] },
      ],
      [ClientMethod.ReadTextFile, { sessionId: "s", path: "/x" }],
      [ClientMethod.WriteTextFile, { sessionId: "s", path: "/x", content: "c" }],
      [ClientMethod.CreateTerminal, { sessionId: "s", command: "ls" }],
      [ClientMethod.TerminalOutput, { sessionId: "s", terminalId: "t" }],
      [ClientMethod.WaitForTerminalExit, { sessionId: "s", terminalId: "t" }],
      [ClientMethod.KillTerminal, { sessionId: "s", terminalId: "t" }],
      [ClientMethod.ReleaseTerminal, { sessionId: "s", terminalId: "t" }],
    ];
    for (const [index, [method, params]] of calls.entries()) {
      const recorder = new Recorder();
      const client = new AcpClient({ channel: recorder });
      client.receive(JSON.stringify({ jsonrpc: "2.0", id: 100 + index, method, params }));
      await Bun.sleep(0);
      expect(recorder.last()).toEqual({
        kind: "error",
        id: 100 + index,
        error: {
          code: RpcErrorCode.MethodNotFound,
          message: `this client does not handle ${method}`,
        },
      });
    }
  });

  test("write_text_file writes and answers null", async () => {
    const recorder = new Recorder();
    let written: unknown;
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        writeTextFile: async (params) => {
          written = params.content;
        },
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 13,
        method: ClientMethod.WriteTextFile,
        params: { sessionId: "s", path: "/x", content: "c" },
      }),
    );
    await Bun.sleep(0);
    expect(written).toBe("c");
    expect(recorder.last()).toEqual({ kind: "result", id: 13, result: null });
  });

  test("the terminal lifecycle is routed and answered", async () => {
    const recorder = new Recorder();
    const calls: string[] = [];
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        createTerminal: async () => {
          calls.push("create");
          return { terminalId: "term-1" };
        },
        terminalOutput: async () => {
          calls.push("output");
          return { output: "ok", truncated: false, exitStatus: { exitCode: 0, signal: null } };
        },
        waitForTerminalExit: async () => {
          calls.push("wait");
          return { exitCode: 0, signal: null };
        },
        killTerminal: async () => {
          calls.push("kill");
        },
        releaseTerminal: async () => {
          calls.push("release");
        },
      },
    });
    const params = { sessionId: "s", terminalId: "term-1" };
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: ClientMethod.CreateTerminal,
        params: { sessionId: "s", command: "ls" },
      }),
    );
    for (const method of [
      ClientMethod.TerminalOutput,
      ClientMethod.WaitForTerminalExit,
      ClientMethod.KillTerminal,
      ClientMethod.ReleaseTerminal,
    ]) {
      client.receive(JSON.stringify({ jsonrpc: "2.0", id: 2, method, params }));
    }
    await Bun.sleep(0);
    expect(calls).toEqual(["create", "output", "wait", "kill", "release"]);
    expect(recorder.last()).toEqual({ kind: "result", id: 2, result: null });
  });

  test("terminal output is decoded before it is sent back", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        terminalOutput: async () =>
          ({
            output: "x",
            truncated: false,
            exitStatus: { exitCode: 0, signal: null },
            extra: "dropped",
          }) as never,
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 14,
        method: ClientMethod.TerminalOutput,
        params: { sessionId: "s", terminalId: "t" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "result",
      id: 14,
      result: { output: "x", truncated: false, exitStatus: { exitCode: 0, signal: null } },
    });
  });

  test("a malformed agent request is a rejected frame", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: { requestPermission: async () => ({ outcome: "cancelled" }) },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 15,
        method: ClientMethod.RequestPermission,
        params: {},
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 15,
      error: {
        code: RpcErrorCode.InvalidParams,
        message: "value has the wrong shape",
      },
    });
  });
});

describe("decoders", () => {
  test("content blocks parse their variants", () => {
    expect(value(decodeContentBlock({ type: "text", text: "a" }))).toEqual({
      type: "text",
      text: "a",
    });
    expect(
      value(decodeContentBlock({ type: "resource_link", uri: "file:///x", name: "x" })),
    ).toEqual({
      type: "resource_link",
      uri: "file:///x",
      name: "x",
    });
    expect(
      value(
        decodeContentBlock({
          type: "resource",
          resource: { uri: "file:///y", text: "body", mimeType: "text/plain" },
        }),
      ),
    ).toEqual({
      type: "resource",
      resource: { uri: "file:///y", text: "body", mimeType: "text/plain" },
    });
    expect(
      value(decodeContentBlock({ type: "resource", resource: { uri: "file:///z", blob: "AAA" } })),
    ).toEqual({
      type: "resource",
      resource: { uri: "file:///z", blob: "AAA" },
    });
  });

  test("an unknown content block is rejected", () => {
    expect(rejection(decodeContentBlock({ type: "image", data: "x" })).code).toBe(
      RpcErrorCode.InvalidRequest,
    );
    expect(rejection(decodeContentBlock("nope")).code).toBe(RpcErrorCode.InvalidRequest);
    expect(rejection(decodeContentBlock({ type: "text" })).code).toBe(RpcErrorCode.InvalidRequest);
    expect(rejection(decodeContentBlock({ type: "resource", resource: {} })).code).toBe(
      RpcErrorCode.InvalidRequest,
    );
  });

  test("every stop reason is recognised and nothing else is", () => {
    for (const reason of STOP_REASONS) {
      expect(isStopReason(reason)).toBe(true);
      expect(value(decodePromptResult({ stopReason: reason }))).toEqual({ stopReason: reason });
    }
    fc.assert(
      fc.property(fc.string(), (value) => {
        expect(isStopReason(value)).toBe((STOP_REASONS as readonly string[]).includes(value));
      }),
    );
    expect(isStopReason(undefined)).toBe(false);
  });

  test("a prompt result without a valid stop reason is rejected", () => {
    expect(rejection(decodePromptResult({ stopReason: "because" })).code).toBe(
      RpcErrorCode.InvalidRequest,
    );
    expect(rejection(decodePromptResult({})).code).toBe(RpcErrorCode.InvalidRequest);
  });

  test("initialize results keep the fields the product reads", () => {
    expect(value(decodeInitializeResult({ protocolVersion: 1 }))).toEqual({ protocolVersion: 1 });
    expect(
      value(
        decodeInitializeResult({
          protocolVersion: 1,
          agentCapabilities: { loadSession: true, promptCapabilities: { image: true } },
          authMethods: [{ id: "login", name: "Log in", description: null }],
        }),
      ),
    ).toEqual({
      protocolVersion: 1,
      agentCapabilities: { loadSession: true, promptCapabilities: { image: true } },
      authMethods: [{ id: "login", name: "Log in", description: null }],
    });
    expect(rejection(decodeInitializeResult({})).code).toBe(RpcErrorCode.InvalidRequest);
  });

  test("new session results keep modes and drop malformed ones", () => {
    expect(value(decodeNewSessionResult({ sessionId: "s" }))).toEqual({ sessionId: "s" });
    expect(
      value(
        decodeNewSessionResult({
          sessionId: "s",
          modes: {
            currentModeId: "ask",
            availableModes: [
              { id: "ask", name: "Ask" },
              { id: "code", name: "Code" },
              { bad: true },
            ],
          },
        }),
      ),
    ).toEqual({
      sessionId: "s",
      modes: {
        currentModeId: "ask",
        availableModes: [
          { id: "ask", name: "Ask" },
          { id: "code", name: "Code" },
        ],
      },
    });
    expect(
      value(decodeNewSessionResult({ sessionId: "s", modes: { currentModeId: "ask" } })),
    ).toEqual({
      sessionId: "s",
    });
  });

  test("optional request fields are dropped when absent and kept when present", () => {
    expect(value(decodeReadTextFileParams({ sessionId: "s", path: "/x" }))).toEqual({
      sessionId: "s",
      path: "/x",
    });
    expect(
      value(decodeReadTextFileParams({ sessionId: "s", path: "/x", line: 3, limit: 9 })),
    ).toEqual({
      sessionId: "s",
      path: "/x",
      line: 3,
      limit: 9,
    });
    expect(
      value(
        decodeCreateTerminalParams({
          sessionId: "s",
          command: "ls",
          args: ["-l"],
          cwd: "/w",
          outputByteLimit: 100,
        }),
      ),
    ).toEqual({ sessionId: "s", command: "ls", args: ["-l"], cwd: "/w", outputByteLimit: 100 });
    expect(value(decodeCreateTerminalParams({ sessionId: "s", command: "ls" }))).toEqual({
      sessionId: "s",
      command: "ls",
    });
  });

  test("terminal output parses an exit status and its absence", () => {
    expect(value(decodeTerminalOutputResult({ output: "x", truncated: false }))).toEqual({
      output: "x",
      truncated: false,
    });
    expect(
      value(
        decodeTerminalOutputResult({
          output: "x",
          truncated: true,
          exitStatus: { exitCode: null, signal: "SIGKILL" },
        }),
      ),
    ).toEqual({ output: "x", truncated: true, exitStatus: { exitCode: null, signal: "SIGKILL" } });
    expect(rejection(decodeTerminalOutputResult({ output: "x" })).code).toBe(
      RpcErrorCode.InvalidRequest,
    );
  });

  test("tool call updates keep only the fields they carry", () => {
    expect(
      value(
        decodeSessionUpdate({ sessionUpdate: "tool_call_update", toolCall: { toolCallId: "t" } }),
      ),
    ).toEqual({
      sessionUpdate: "tool_call_update",
      toolCall: { toolCallId: "t" },
    });
    expect(
      value(
        decodeSessionUpdate({
          sessionUpdate: "tool_call_update",
          toolCall: { toolCallId: "t", title: null, kind: null, status: null },
        }),
      ),
    ).toEqual({
      sessionUpdate: "tool_call_update",
      toolCall: { toolCallId: "t", title: null, kind: null, status: null },
    });
  });

  test("session info updates keep null fields", () => {
    expect(value(decodeSessionUpdate({ sessionUpdate: "session_info_update" }))).toEqual({
      sessionUpdate: "session_info_update",
    });
    expect(
      value(
        decodeSessionUpdate({ sessionUpdate: "session_info_update", title: null, updatedAt: null }),
      ),
    ).toEqual({
      sessionUpdate: "session_info_update",
      title: null,
      updatedAt: null,
    });
  });

  test("a tool call without a valid status drops the status", () => {
    expect(
      value(
        decodeSessionUpdate({
          sessionUpdate: "tool_call",
          toolCall: { toolCallId: "t", title: "x", status: "bogus" },
        }),
      ),
    ).toEqual({ sessionUpdate: "tool_call", toolCall: { toolCallId: "t", title: "x" } });
  });

  test("permission options must use a known kind", () => {
    expect(rejection(decodeRequestPermissionParams({ sessionId: "s", options: [] })).code).toBe(
      RpcErrorCode.InvalidRequest,
    );
    expect(
      rejection(
        decodeRequestPermissionParams({
          sessionId: "s",
          toolCall: { toolCallId: "t" },
          options: [{ optionId: "a", name: "A", kind: "maybe" }],
        }),
      ).code,
    ).toBe(RpcErrorCode.InvalidRequest);
  });

  test("terminal refs need both ids", () => {
    expect(rejection(decodeTerminalRefParams({ sessionId: "s" })).code).toBe(
      RpcErrorCode.InvalidRequest,
    );
    expect(value(decodeTerminalRefParams({ sessionId: "s", terminalId: "t" }))).toEqual({
      sessionId: "s",
      terminalId: "t",
    });
  });

  test("malformed updates are rejected", () => {
    const malformed: unknown[] = [
      {},
      { sessionUpdate: 5 },
      { sessionUpdate: "agent_message_chunk" },
      { sessionUpdate: "plan", entries: "x" },
      { sessionUpdate: "plan", entries: [{}] },
      { sessionUpdate: "tool_call", toolCall: {} },
      { sessionUpdate: "tool_call_update" },
      { sessionUpdate: "current_mode_update" },
      { sessionUpdate: "available_commands_update" },
      { sessionUpdate: "available_commands_update", availableCommands: [{}] },
      { sessionUpdate: "usage_update", used: "x" },
    ];
    for (const value of malformed) {
      expect(rejection(decodeSessionUpdate(value)).code).toBe(RpcErrorCode.InvalidRequest);
    }
  });
});

describe("wire shape", () => {
  test("prompt sends the session id and blocks the agent expects", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    void client.prompt("sess-9", [{ type: "text", text: "do it" }]);
    expect(recorder.last()).toEqual({
      kind: "request",
      id: 1,
      method: AgentMethod.Prompt,
      params: { sessionId: "sess-9", prompt: [{ type: "text", text: "do it" }] },
    });
  });

  test("newSession sends the caller params unchanged", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    void client.newSession({ cwd: "/work", mcpServers: [] });
    expect(recorder.last()).toEqual({
      kind: "request",
      id: 1,
      method: AgentMethod.NewSession,
      params: { cwd: "/work", mcpServers: [] },
    });
  });

  test("initialize sends the caller params unchanged", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    void client.initialize({ protocolVersion: 1 });
    expect(recorder.last()).toEqual({
      kind: "request",
      id: 1,
      method: AgentMethod.Initialize,
      params: { protocolVersion: 1 },
    });
  });

  test("clientInfo is sent only when configured", () => {
    const recorder = new Recorder();
    const withInfo = new AcpClient({ channel: recorder, clientInfo: { name: "n", version: "v" } });
    expect(withInfo.defaultInitializeParams().clientInfo).toEqual({ name: "n", version: "v" });
    const without = new AcpClient({ channel: recorder });
    expect(without.defaultInitializeParams().clientInfo).toBeUndefined();
  });

  test("an update carries params without an update key is dropped, not thrown", () => {
    const recorder = new Recorder();
    const seen: unknown[] = [];
    const client = new AcpClient({
      channel: recorder,
      handlers: { onSessionUpdate: (u) => seen.push(u) },
    });
    // A frame with no update is dropped: the union means nothing throws.
    expect(() =>
      client.receive(JSON.stringify({ jsonrpc: "2.0", method: ClientMethod.SessionUpdate })),
    ).not.toThrow();
    expect(seen).toEqual([]);
  });

  test("a route name that is not a client method is refused", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive(
      JSON.stringify({ jsonrpc: "2.0", id: 77, method: AgentMethod.Prompt, params: {} }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 77,
      error: {
        code: RpcErrorCode.MethodNotFound,
        message: "this client does not handle session/prompt",
      },
    });
  });

  test("a prototype key is not treated as a route", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 78, method: "toString", params: {} }));
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 78,
      error: { code: RpcErrorCode.MethodNotFound, message: "this client does not handle toString" },
    });
  });

  test("a non-Error rejection from a route becomes an internal error", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: { readTextFile: () => Promise.reject({ reason: "odd" }) },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 79,
        method: ClientMethod.ReadTextFile,
        params: { sessionId: "s", path: "/x" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 79,
      error: { code: RpcErrorCode.InternalError, message: "[object Object]" },
    });
  });
});

describe("diagnostics and defaults", () => {
  test("the default initialize params have no clientInfo key at all", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    expect(Object.keys(client.defaultInitializeParams())).toEqual([
      "protocolVersion",
      "clientCapabilities",
    ]);
  });

  test("a handler that throws an error-shaped object is still an internal error", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        readTextFile: () => Promise.reject({ code: 42, message: "from the handler" }),
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 80,
        method: ClientMethod.ReadTextFile,
        params: { sessionId: "s", path: "/x" },
      }),
    );
    await Bun.sleep(0);
    expect(recorder.last()).toEqual({
      kind: "error",
      id: 80,
      error: { code: RpcErrorCode.InternalError, message: "[object Object]" },
    });
  });

  test("create_terminal keeps the environment it is given", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    void client.request("noop", {});
    expect(
      value(
        decodeCreateTerminalParams({
          sessionId: "s",
          command: "ls",
          env: [{ name: "A", value: "1" }],
        }),
      ),
    ).toEqual({ sessionId: "s", command: "ls", env: [{ name: "A", value: "1" }] });
  });
});

describe("notification routing", () => {
  test("a session update with a handler and no params is dropped, not thrown", () => {
    const recorder = new Recorder();
    let called = 0;
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        onSessionUpdate: () => {
          called++;
        },
      },
    });
    expect(() =>
      client.receive(JSON.stringify({ jsonrpc: "2.0", method: ClientMethod.SessionUpdate })),
    ).not.toThrow();
    expect(called).toBe(0);
  });

  test("a session update with non-object params is dropped by the same path", () => {
    const recorder = new Recorder();
    let called = 0;
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        onSessionUpdate: () => {
          called++;
        },
      },
    });
    expect(() =>
      client.receive(
        JSON.stringify({ jsonrpc: "2.0", method: ClientMethod.SessionUpdate, params: "x" }),
      ),
    ).not.toThrow();
    expect(called).toBe(0);
  });

  test("another agent method never reaches the update handler", () => {
    const recorder = new Recorder();
    let called = 0;
    const client = new AcpClient({
      channel: recorder,
      handlers: {
        onSessionUpdate: () => {
          called++;
        },
      },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        method: ClientMethod.SessionUpdate + "/other",
        params: { update: {} },
      }),
    );
    expect(called).toBe(0);
  });
});

describe("a notification without params", () => {
  test("null params read as absent, so the update is dropped and nothing throws", () => {
    const recorder = new Recorder();
    const seen: unknown[] = [];
    const client = new AcpClient({
      channel: recorder,
      handlers: { onSessionUpdate: (update) => seen.push(update) },
    });
    expect(() =>
      client.receive(
        JSON.stringify({ jsonrpc: "2.0", method: ClientMethod.SessionUpdate, params: null }),
      ),
    ).not.toThrow();
    expect(seen).toEqual([]);
  });
});

describe("the exact decode branch", () => {
  test("a decoded result carries its value, not its wrapper", () => {
    expect(value(decodePromptResult({ stopReason: "end_turn" }))).toEqual({
      stopReason: "end_turn",
    });
  });

  test("a malformed frame is dropped before the switch, so nothing is sent back", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    expect(() => client.receive("not json")).not.toThrow();
    expect(recorder.sent).toEqual([]);
  });

  test("an update with no handler is dropped without a throw", () => {
    const client = new AcpClient({ channel: new Recorder() });
    expect(() =>
      client.receive(
        JSON.stringify({
          jsonrpc: "2.0",
          method: ClientMethod.SessionUpdate,
          params: { update: {} },
        }),
      ),
    ).not.toThrow();
  });

  test("a handler receives the update the decoder accepted", () => {
    const seen: unknown[] = [];
    const client = new AcpClient({
      channel: new Recorder(),
      handlers: { onSessionUpdate: (update) => seen.push(update) },
    });
    client.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        method: ClientMethod.SessionUpdate,
        params: {
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: "hi" },
          },
        },
      }),
    );
    expect(seen).toEqual([
      { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hi" } },
    ]);
  });
});

describe("a frame that is not a message reaches no handler", () => {
  test("a malformed frame sends nothing and leaves no pending request", async () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive("{oops");
    await Bun.sleep(0);
    expect(recorder.sent).toEqual([]);
    expect(client.pendingCount).toBe(0);
  });

  test("a well-formed frame with an unknown method still settles nothing", () => {
    const recorder = new Recorder();
    const client = new AcpClient({ channel: recorder });
    client.receive(JSON.stringify({ jsonrpc: "2.0", id: 99, result: 1 }));
    expect(recorder.sent).toEqual([]);
  });
});
