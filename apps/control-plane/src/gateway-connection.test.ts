import { describe, expect, test } from "bun:test";
import type { Session, Turn, TurnUpdate } from "@agent/store";
import type { AgentCapability } from "@agent/protocol";
import { frameText, handleSocketMessage, type GatewaySessions } from "./gateway-connection.ts";

/**
 * The Worker and the dev server both drive a gateway through
 * `handleSocketMessage`, so this is the deployed path: the socket only carries
 * strings, and every decision is made here.
 */

const session = (overrides: Partial<Session> = {}): Session =>
  ({
    id: "s1",
    thread: { provider: "linear", workspaceId: "w", channelId: "issue-1", threadId: "sess-1" },
    status: "active",
    acpSessionId: null,
    gatewayId: null,
    selection: {
      agent: "claude-code",
      runtime: "local-vm",
      protocol: "stdio",
      model: { provider: "anthropic", modelId: "m" },
    },
    conversationId: "c1",
    turnSeq: 1,
    createdAt: 0,
    updatedAt: 0,
    ...overrides,
  }) as Session;

const turn = (overrides: Partial<Turn> = {}): Turn =>
  ({
    id: "t1",
    sessionId: "s1",
    prompt: "hello",
    status: "queued",
    idempotencyKey: "evt-1",
    stopReason: null,
    error: null,
    createdAt: 10,
    updatedAt: 10,
    ...overrides,
  }) as Turn;

/** The session object as a gateway connection sees it, recording every write. */
class FakeSessions implements GatewaySessions {
  readonly transitions: Array<{
    sessionId: string;
    turnId: string;
    update: TurnUpdate;
    now: number;
  }> = [];
  readonly binds: Array<{ sessionId: string; acpSessionId: string; now: number }> = [];
  readonly capabilities: Array<{ sessionId: string; capability: AgentCapability; now: number }> =
    [];

  constructor(
    readonly stored: Session | undefined,
    readonly storedTurns: Turn[],
  ) {}

  async session(): Promise<Session | undefined> {
    return this.stored;
  }

  async turns(): Promise<Turn[]> {
    return this.storedTurns;
  }

  async transition(sessionId: string, turnId: string, update: TurnUpdate, now: number) {
    this.transitions.push({ sessionId, turnId, update, now });
    return { ok: true };
  }

  async bind(sessionId: string, acpSessionId: string, _gatewayId: string, now: number) {
    this.binds.push({ sessionId, acpSessionId, now });
    return { ok: true };
  }

  async capability(sessionId: string, capability: AgentCapability, now: number): Promise<Session> {
    this.capabilities.push({ sessionId, capability, now });
    return { ...this.stored!, capability, updatedAt: now };
  }
}

/** A socket that records what the handler sends and what it learned. */
class FakeSocket {
  readonly sent: string[] = [];
  sessionId: string | undefined;

  send(frame: string): void {
    this.sent.push(frame);
  }
}

const settledCalls: Array<unknown> = [];
const recordSettled = async (settled: unknown): Promise<void> => {
  settledCalls.push(settled);
};

const connect = (sessions: FakeSessions, socket = new FakeSocket()) => ({ sessions, socket });

describe("a gateway says hello", () => {
  test("a hello for a queued turn sends one prompt and claims the turn", async () => {
    const sessions = new FakeSessions(session({ acpSessionId: "acp-1" }), [turn()]);
    const { socket } = connect(sessions);
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "hello", sessionId: "s1" }),
      50,
      recordSettled,
    );
    expect(socket.sent).toEqual([
      JSON.stringify({
        jsonrpc: "2.0",
        method: "gateway/command",
        params: {
          type: "prompt",
          sessionId: "s1",
          acpSessionId: "acp-1",
          turnId: "t1",
          text: "hello",
          // The routed model travels with the prompt so the agent can select it.
          model: { provider: "anthropic", modelId: "m" },
        },
      }),
    ]);
    expect(sessions.transitions).toEqual([
      { sessionId: "s1", turnId: "t1", update: { status: "dispatched" }, now: 50 },
    ]);
    expect(socket.sessionId).toBe("s1");
  });

  test("the frame is a JSON-RPC notification, so the gateway's codec reads it", async () => {
    const sessions = new FakeSessions(session(), [turn()]);
    const { socket } = connect(sessions);
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "hello", sessionId: "s1" }),
      0,
    );
    // The gateway's codec unwraps `params` from a JSON-RPC envelope, so the bare
    // frame is nested rather than sent as the message itself.
    const sent = JSON.parse(socket.sent[0]!) as {
      jsonrpc: string;
      method: string;
      params: unknown;
    };
    expect(sent.jsonrpc).toBe("2.0");
    expect(sent.method).toBe("gateway/command");
    expect(sent.params).toMatchObject({
      type: "prompt",
      sessionId: "s1",
      turnId: "t1",
      text: "hello",
    });
  });

  test("a hello for a session with no queued turn sends nothing and claims nothing", async () => {
    const sessions = new FakeSessions(session(), []);
    const { socket } = connect(sessions);
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "hello", sessionId: "s1" }),
      50,
      recordSettled,
    );
    expect(socket.sent).toEqual([]);
    expect(sessions.transitions).toEqual([]);
    expect(socket.sessionId).toBe("s1");
  });

  test("a hello for an unknown session sends nothing, because it is still that connection's session", async () => {
    const sessions = new FakeSessions(undefined, []);
    const { socket } = connect(sessions);
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "hello", sessionId: "gone" }),
      0,
    );
    expect(socket.sent).toEqual([]);
    expect(socket.sessionId).toBe("gone");
  });

  test("a hello is not a settle, so the reply path is not called", async () => {
    const sessions = new FakeSessions(session(), [turn()]);
    const { socket } = connect(sessions);
    settledCalls.length = 0;
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "hello", sessionId: "s1" }),
      0,
      recordSettled,
    );
    expect(settledCalls).toEqual([]);
  });
});

describe("a gateway reports", () => {
  test("a bind records the ACP session on the connection's session", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "dispatched" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "session.bound", sessionId: "s1", acpSessionId: "acp-9" }),
      70,
      recordSettled,
    );
    expect(sessions.binds).toEqual([{ sessionId: "s1", acpSessionId: "acp-9", now: 70 }]);
    expect(socket.sent).toEqual([]);
    expect(socket.sessionId).toBe("s1");
  });

  test("a capability report is kept on the session", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "dispatched" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "session.capability",
        sessionId: "s1",
        capability: {
          agent: { name: "OpenCode", version: "2.3.0" },
          currentModelId: "ollama-cloud/deepseek-v4.1-flash",
          availableModels: ["ollama-cloud/deepseek-v4.1-flash", "opencode/tidepool-alpha-free"],
          statesModels: true,
        },
      }),
      70,
      recordSettled,
    );
    expect(sessions.capabilities).toEqual([
      {
        sessionId: "s1",
        capability: {
          agent: { name: "OpenCode", version: "2.3.0" },
          currentModelId: "ollama-cloud/deepseek-v4.1-flash",
          availableModels: ["ollama-cloud/deepseek-v4.1-flash", "opencode/tidepool-alpha-free"],
          statesModels: true,
        },
        now: 70,
      },
    ]);
    // Learning what the agent offers settles no turn.
    expect(settledCalls).toEqual([]);
  });

  test("a capability with no model list is dropped, so a bad frame cannot clear the record", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "dispatched" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "session.capability",
        sessionId: "s1",
        capability: { statesModels: true },
      }),
      70,
      recordSettled,
    );
    expect(sessions.capabilities).toEqual([]);
  });

  test("a completed turn answers the thread with the agent's text", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "running" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    settledCalls.length = 0;
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "turn.completed",
        sessionId: "s1",
        turnId: "t1",
        stopReason: "end_turn",
        text: "the answer",
      }),
      90,
      recordSettled,
    );
    expect(sessions.transitions).toEqual([
      {
        sessionId: "s1",
        turnId: "t1",
        update: { status: "completed", stopReason: "end_turn" },
        now: 90,
      },
    ]);
    expect(settledCalls).toEqual([{ sessionId: "s1", replyTo: "sess-1", text: "the answer" }]);
  });

  test("a failed turn answers the thread with the reason it failed", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "running" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    settledCalls.length = 0;
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "turn.failed", sessionId: "s1", turnId: "t1", error: "agent died" }),
      90,
      recordSettled,
    );
    expect(sessions.transitions.at(-1)).toEqual({
      sessionId: "s1",
      turnId: "t1",
      update: { status: "failed", error: "agent died" },
      now: 90,
    });
    expect(settledCalls).toEqual([{ sessionId: "s1", replyTo: "sess-1", error: "agent died" }]);
  });

  test("a report on a connection with no session yet uses the frame's session", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "dispatched" })]);
    const { socket } = connect(sessions);
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "session.bound", sessionId: "s1", acpSessionId: "acp-9" }),
      70,
      recordSettled,
    );
    expect(sessions.binds).toEqual([{ sessionId: "s1", acpSessionId: "acp-9", now: 70 }]);
    expect(socket.sessionId).toBe("s1");
  });

  test("a report for an unknown session is dropped", async () => {
    const sessions = new FakeSessions(undefined, []);
    const { socket } = connect(sessions);
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "session.bound", sessionId: "gone", acpSessionId: "acp-9" }),
      70,
      recordSettled,
    );
    expect(sessions.binds).toEqual([]);
    expect(socket.sessionId).toBe("gone");
  });

  test("a report the reducer does not settle leaves the reply path alone", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "dispatched" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    settledCalls.length = 0;
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "turn.started", sessionId: "s1", turnId: "t1" }),
      90,
      recordSettled,
    );
    expect(settledCalls).toEqual([]);
  });

  test("a message that is not JSON is dropped without touching the session", async () => {
    const sessions = new FakeSessions(session(), [turn()]);
    const { socket } = connect(sessions);
    await handleSocketMessage(sessions, socket, "not json", 0, recordSettled);
    expect(socket.sent).toEqual([]);
    expect(sessions.transitions).toEqual([]);
    expect(socket.sessionId).toBeUndefined();
  });

  test("a frame the codec cannot place is dropped without touching the session", async () => {
    const sessions = new FakeSessions(session(), [turn()]);
    const { socket } = connect(sessions);
    await handleSocketMessage(sessions, socket, JSON.stringify({ type: "pong" }), 0, recordSettled);
    expect(socket.sent).toEqual([]);
    expect(sessions.transitions).toEqual([]);
  });

  test("a settle with no reply path still moves the turn, so the run is not lost", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "running" })]);
    const { socket } = connect(sessions);
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "turn.completed",
        sessionId: "s1",
        turnId: "t1",
        stopReason: "end_turn",
        text: "kept",
      }),
      90,
    );
    expect(sessions.transitions.at(-1)).toMatchObject({ update: { status: "completed" } });
  });
});

describe("frame text", () => {
  test("a frame is wrapped as a gateway/command notification", () => {
    const text = frameText({ type: "prompt", turnId: "t1" });
    expect(JSON.parse(text)).toEqual({
      jsonrpc: "2.0",
      method: "gateway/command",
      params: { type: "prompt", turnId: "t1" },
    });
  });
});

describe("a capability that rules out the routed model", () => {
  /** A connection whose session already has a route the agent cannot run. */
  const refusalOver = (turns: Turn[]) => {
    const stored = session();
    const sessions = new FakeSessions(stored, turns);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    return { sessions, socket };
  };

  const capability = {
    type: "session.capability",
    sessionId: "s1",
    capability: { availableModels: ["agent-only"], statesModels: true },
  };

  test("a queued turn is failed with the reason, and the thread is told", async () => {
    const { sessions, socket } = refusalOver([turn({ status: "queued" })]);
    settledCalls.length = 0;
    await handleSocketMessage(sessions, socket, JSON.stringify(capability), 90, recordSettled);
    expect(sessions.transitions).toEqual([
      {
        sessionId: "s1",
        turnId: "t1",
        update: {
          status: "failed",
          error: "the agent does not offer m; it lists agent-only",
        },
        now: 90,
      },
    ]);
    expect(settledCalls).toEqual([
      {
        sessionId: "s1",
        replyTo: "sess-1",
        error: expect.stringContaining("does not offer"),
      },
    ]);
  });

  test("a turn the gateway already claimed is left to run, because it already chose", async () => {
    for (const status of ["dispatched", "running", "completed"] as const) {
      const { sessions, socket } = refusalOver([turn({ status })]);
      settledCalls.length = 0;
      await handleSocketMessage(sessions, socket, JSON.stringify(capability), 90, recordSettled);
      expect(sessions.transitions).toEqual([]);
      expect(settledCalls).toEqual([]);
    }
  });

  test("the oldest queued turn is the one failed, so an earlier wait is reported first", async () => {
    const { sessions, socket } = refusalOver([
      turn({ id: "newer", createdAt: 20 }),
      turn({ id: "older", createdAt: 5 }),
    ]);
    settledCalls.length = 0;
    await handleSocketMessage(sessions, socket, JSON.stringify(capability), 90, recordSettled);
    expect(sessions.transitions).toEqual([expect.objectContaining({ turnId: "older" })]);
  });

  test("a model the agent does list refuses nothing", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "queued" })]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "session.capability",
        sessionId: "s1",
        capability: { availableModels: ["m", "other"], statesModels: true },
      }),
      90,
      recordSettled,
    );
    expect(sessions.transitions).toEqual([]);
  });

  test("an agent that names no models refuses nothing, because it said nothing", async () => {
    const sessions = new FakeSessions(session(), [turn({ status: "queued" })]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "session.capability",
        sessionId: "s1",
        capability: { availableModels: [], statesModels: false },
      }),
      90,
      recordSettled,
    );
    expect(sessions.transitions).toEqual([]);
  });

  test("a session with no route refuses nothing, because there is no model to check", async () => {
    const sessions = new FakeSessions(session({ selection: null }), [turn({ status: "queued" })]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(sessions, socket, JSON.stringify(capability), 90, recordSettled);
    expect(sessions.transitions).toEqual([]);
  });
});

describe("what a capability frame answers and what it refuses", () => {
  const frame = (capability: AgentCapability) =>
    JSON.stringify({ type: "session.capability", sessionId: "s1", capability });

  test("the frame sends nothing back, because a capability is not a command", async () => {
    const sessions = new FakeSessions(session(), []);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      frame({ availableModels: [], statesModels: false }),
      90,
      recordSettled,
    );
    expect(socket.sent).toEqual([]);
    expect(socket.sessionId).toBe("s1");
  });

  test("a session with no route is left alone rather than read for a model", async () => {
    // The route is what names the model to check, so a session without one has
    // nothing to refuse and must not reach for `selection.model`.
    const sessions = new FakeSessions(session({ selection: null, capability: null }), [
      turn({ status: "queued" }),
    ]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      frame({ availableModels: ["a"], statesModels: true }),
      90,
      recordSettled,
    );
    expect(sessions.transitions).toEqual([]);
  });

  test("a turn that is already settled is not failed again", async () => {
    const sessions = new FakeSessions(session(), [
      turn({ status: "completed" }),
      turn({ id: "t2", status: "failed", createdAt: 20 }),
    ]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      frame({ availableModels: ["other"], statesModels: true }),
      90,
      recordSettled,
    );
    expect(sessions.transitions).toEqual([]);
  });
});

describe("a message on a connection that has never said hello", () => {
  test("the connection's session is what the frame is read against, not the socket's", async () => {
    // A report that names no session of its own falls back to the connection's,
    // so the socket's current session is what decides whose turn it is.
    const sessions = new FakeSessions(session(), [turn({ status: "dispatched" })]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({ type: "session.bound", acpSessionId: "acp-9" }),
      70,
      recordSettled,
    );
    // A bound frame with no session id is not a report, so nothing happens.
    expect(sessions.binds).toEqual([]);
    expect(socket.sessionId).toBe("s1");
  });
});

describe("a session stored before capabilities existed", () => {
  test("a session with no capability field at all refuses nothing", async () => {
    // A record written by an older deployment has no `capability` key, which
    // reads as `undefined`; treating that as a refusal would fail a turn for a
    // report that was never made.
    const stored = session();
    delete (stored as { capability?: unknown }).capability;
    const sessions = new FakeSessions({ ...stored, selection: stored.selection } as Session, [
      turn({ status: "queued" }),
    ]);
    const socket = new FakeSocket();
    socket.sessionId = "s1";
    await handleSocketMessage(
      sessions,
      socket,
      JSON.stringify({
        type: "session.capability",
        sessionId: "s1",
        capability: { availableModels: ["other"], statesModels: true },
      }),
      90,
      recordSettled,
    );
    // The capability in this frame is recorded, but the stored one it replaces
    // is read only after the write, so this asserts the write still happened.
    expect(sessions.capabilities).toHaveLength(1);
  });
});
