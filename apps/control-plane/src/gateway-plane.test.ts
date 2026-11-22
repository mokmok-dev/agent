import { describe, expect, test } from "bun:test";
import type { Session, Turn } from "@agent/store";
import { effectOf, helloOf, nextPrompt, promptFor, reportOf } from "./gateway-plane.ts";

/**
 * The reducer and the dispatch decision, driven directly. These are the rules
 * the gateway path depends on, so each branch is pinned by name rather than
 * reached only through a socket.
 */

const session = (overrides: Partial<Session> = {}): Session =>
  ({
    id: "s1",
    thread: { provider: "linear", workspaceId: "w", channelId: "i", threadId: "sess" },
    status: "active",
    acpSessionId: null,
    gatewayId: null,
    selection: null,
    conversationId: null,
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

describe("choosing what to dispatch", () => {
  test("a frame carries the session, its ACP id, the turn, and the text", () => {
    expect(promptFor(session({ acpSessionId: "acp-1" }), turn())).toEqual({
      type: "prompt",
      sessionId: "s1",
      acpSessionId: "acp-1",
      turnId: "t1",
      text: "hello",
    });
  });

  test("a session with no ACP id yet sends an empty one, which the gateway opens", () => {
    expect(promptFor(session(), turn()).acpSessionId).toBe("");
  });

  test("the oldest queued turn is chosen, not the newest", () => {
    const frame = nextPrompt(session(), [
      turn({ id: "newer", createdAt: 20 }),
      turn({ id: "older", createdAt: 5 }),
    ]);
    expect(frame?.turnId).toBe("older");
  });

  test("a turn that is dispatched, running, or settled is not queued", () => {
    for (const status of ["dispatched", "running", "completed", "failed", "cancelled"] as const) {
      expect(nextPrompt(session(), [turn({ status })])).toBeUndefined();
    }
  });

  test("no turns means nothing to send", () => {
    expect(nextPrompt(session(), [])).toBeUndefined();
  });
});

describe("the effect of a gateway report", () => {
  test("a bind records the ACP session", () => {
    expect(
      effectOf({ type: "session.bound", sessionId: "s1", acpSessionId: "acp-9" }, session(), []),
    ).toEqual({ kind: "bind", acpSessionId: "acp-9" });
  });

  test("an empty ACP session id is refused, because it would lose the binding", () => {
    expect(
      effectOf({ type: "session.bound", sessionId: "s1", acpSessionId: "" }, session(), []),
    ).toEqual({
      kind: "ignore",
      reason: "an empty ACP session id would lose the binding",
    });
  });

  test("a start on a dispatched turn moves it to running", () => {
    expect(
      effectOf({ type: "turn.started", sessionId: "s1", turnId: "t1" }, session(), [
        turn({ status: "dispatched" }),
      ]),
    ).toEqual({ kind: "settle", turnId: "t1", update: { status: "running" } });
  });

  test("a start on a queued turn is premature and ignored", () => {
    expect(
      effectOf({ type: "turn.started", sessionId: "s1", turnId: "t1" }, session(), [turn()]),
    ).toEqual({
      kind: "ignore",
      reason: "turn is queued, not dispatched",
    });
  });

  test("a start for a turn the session does not have is ignored", () => {
    expect(
      effectOf({ type: "turn.started", sessionId: "s1", turnId: "nope" }, session(), [turn()]),
    ).toEqual({
      kind: "ignore",
      reason: "no such turn",
    });
  });

  test("a completion settles a running turn and carries the text", () => {
    expect(
      effectOf(
        {
          type: "turn.completed",
          sessionId: "s1",
          turnId: "t1",
          stopReason: "end_turn",
          text: "the answer",
        },
        session(),
        [turn({ status: "running" })],
      ),
    ).toEqual({
      kind: "settle",
      turnId: "t1",
      update: { status: "completed", stopReason: "end_turn" },
      text: "the answer",
    });
  });

  test("a completion with no text carries no text field", () => {
    const effect = effectOf(
      { type: "turn.completed", sessionId: "s1", turnId: "t1", stopReason: "end_turn" },
      session(),
      [turn({ status: "running" })],
    );
    expect(effect).toEqual({
      kind: "settle",
      turnId: "t1",
      update: { status: "completed", stopReason: "end_turn" },
    });
    expect("text" in effect).toBe(false);
  });

  test("a completion on a dispatched turn is ignored, because the state table needs running", () => {
    expect(
      effectOf(
        { type: "turn.completed", sessionId: "s1", turnId: "t1", stopReason: "end_turn" },
        session(),
        [turn({ status: "dispatched" })],
      ),
    ).toEqual({ kind: "ignore", reason: "turn is dispatched, not running" });
  });

  test("a completion for a turn the session does not have is ignored", () => {
    expect(
      effectOf(
        { type: "turn.completed", sessionId: "s1", turnId: "nope", stopReason: "end_turn" },
        session(),
        [turn({ status: "running" })],
      ),
    ).toEqual({ kind: "ignore", reason: "no such turn" });
  });

  test("a failure settles a running turn with its reason", () => {
    expect(
      effectOf({ type: "turn.failed", sessionId: "s1", turnId: "t1", error: "boom" }, session(), [
        turn({ status: "running" }),
      ]),
    ).toEqual({ kind: "settle", turnId: "t1", update: { status: "failed", error: "boom" } });
  });

  test("a failure settles a dispatched turn too, so a crash before the start event still reports", () => {
    expect(
      effectOf({ type: "turn.failed", sessionId: "s1", turnId: "t1", error: "boom" }, session(), [
        turn({ status: "dispatched" }),
      ]),
    ).toEqual({ kind: "settle", turnId: "t1", update: { status: "failed", error: "boom" } });
  });

  test("a failure for an already settled turn is ignored", () => {
    for (const status of ["completed", "failed"] as const) {
      expect(
        effectOf({ type: "turn.failed", sessionId: "s1", turnId: "t1", error: "late" }, session(), [
          turn({ status }),
        ]),
      ).toEqual({ kind: "ignore", reason: `turn is ${status}, already settled` });
    }
  });

  test("a failure for a turn the session does not have is ignored", () => {
    expect(
      effectOf({ type: "turn.failed", sessionId: "s1", turnId: "nope", error: "x" }, session(), []),
    ).toEqual({ kind: "ignore", reason: "no such turn" });
  });

  test("any report for another session is ignored first", () => {
    expect(
      effectOf(
        { type: "turn.completed", sessionId: "other", turnId: "t1", stopReason: "end_turn" },
        session(),
        [turn({ status: "running" })],
      ),
    ).toEqual({ kind: "ignore", reason: "event is for another session" });
  });
});

describe("reading a gateway's payload", () => {
  test("a JSON-RPC gateway/event notification is unwrapped", () => {
    expect(
      reportOf({
        jsonrpc: "2.0",
        method: "gateway/event",
        params: { type: "turn.started", sessionId: "s1", turnId: "t1" },
      }),
    ).toEqual({ type: "turn.started", sessionId: "s1", turnId: "t1" });
  });

  test("a bare object is read too", () => {
    expect(reportOf({ type: "turn.started", sessionId: "s1", turnId: "t1" })).toEqual({
      type: "turn.started",
      sessionId: "s1",
      turnId: "t1",
    });
  });

  test("a notification whose params are not an object is not a report", () => {
    expect(reportOf({ jsonrpc: "2.0", method: "gateway/event", params: 1 })).toBeUndefined();
  });

  test("a missing session id is not a report", () => {
    expect(reportOf({ type: "turn.started", turnId: "t1" })).toBeUndefined();
  });

  test("a completed event defaults its stop reason rather than failing", () => {
    expect(reportOf({ type: "turn.completed", sessionId: "s1", turnId: "t1" })).toEqual({
      type: "turn.completed",
      sessionId: "s1",
      turnId: "t1",
      stopReason: "end_turn",
    });
  });

  test("a failed event with no reason still names a failure", () => {
    expect(reportOf({ type: "turn.failed", sessionId: "s1", turnId: "t1" })).toEqual({
      type: "turn.failed",
      sessionId: "s1",
      turnId: "t1",
      error: "the gateway reported a failure with no reason",
    });
  });

  test("a completed event without a turn id is not a report", () => {
    expect(reportOf({ type: "turn.completed", sessionId: "s1" })).toBeUndefined();
  });

  test("a failed event without a turn id is not a report", () => {
    expect(reportOf({ type: "turn.failed", sessionId: "s1" })).toBeUndefined();
  });

  test("a bind without an ACP id is not a report", () => {
    expect(reportOf({ type: "session.bound", sessionId: "s1" })).toBeUndefined();
  });

  test("a bind with no session is not a report", () => {
    expect(reportOf({ type: "session.bound", acpSessionId: "a" })).toBeUndefined();
  });

  test("an unknown type is not a report", () => {
    expect(reportOf({ type: "turn.update", sessionId: "s1" })).toBeUndefined();
  });

  test("a non-object is not a report", () => {
    expect(reportOf("hello")).toBeUndefined();
    expect(reportOf(null)).toBeUndefined();
  });

  test("a hello names its session, and any other frame does not", () => {
    expect(helloOf({ type: "hello", sessionId: "s1" })).toBe("s1");
    expect(
      helloOf({
        jsonrpc: "2.0",
        method: "gateway/event",
        params: { type: "hello", sessionId: "s1" },
      }),
    ).toBe("s1");
    expect(helloOf({ type: "hello" })).toBeUndefined();
    expect(helloOf({ type: "prompt", sessionId: "s1" })).toBeUndefined();
    expect(helloOf(undefined)).toBeUndefined();
  });
});

describe("the payload edges", () => {
  test("a non-string field is not read as a string", () => {
    expect(reportOf({ type: "turn.started", sessionId: 1, turnId: "t1" })).toBeUndefined();
    expect(reportOf({ type: "turn.started", sessionId: "s1", turnId: 2 })).toBeUndefined();
  });

  test("a notification whose params are a non-object value is not a report", () => {
    expect(reportOf({ jsonrpc: "2.0", method: "gateway/event", params: "hello" })).toBeUndefined();
  });

  test("a hello whose session is not a string is not a hello", () => {
    expect(helloOf({ type: "hello", sessionId: 1 })).toBeUndefined();
  });

  test("a turn id that is absent is not a start", () => {
    expect(reportOf({ type: "turn.started", sessionId: "s1" })).toBeUndefined();
  });

  test("the stop reason a gateway states is kept, not replaced", () => {
    expect(
      reportOf({ type: "turn.completed", sessionId: "s1", turnId: "t1", stopReason: "max_tokens" }),
    ).toMatchObject({ stopReason: "max_tokens" });
  });

  test("a text field that is present travels with the completion", () => {
    const report = reportOf({ type: "turn.completed", sessionId: "s1", turnId: "t1", text: "" });
    expect(report).toEqual({
      type: "turn.completed",
      sessionId: "s1",
      turnId: "t1",
      stopReason: "end_turn",
      text: "",
    });
  });
});

describe("the exact payload shape", () => {
  test("a notification's params object is unwrapped, and a null one is not", () => {
    expect(reportOf({ jsonrpc: "2.0", method: "gateway/event", params: null })).toBeUndefined();
  });

  test("an array is an object to typeof, so the params guard must also reject it", () => {
    // An array has no `type`, so this must not be read as a report.
    expect(reportOf({ jsonrpc: "2.0", method: "gateway/event", params: [] })).toBeUndefined();
  });

  test("the turn id decides which turn settles, not merely that one exists", () => {
    const effect = effectOf({ type: "turn.started", sessionId: "s1", turnId: "t2" }, session(), [
      turn({ id: "t1", status: "dispatched" }),
      turn({ id: "t2", status: "dispatched" }),
    ]);
    expect(effect).toEqual({ kind: "settle", turnId: "t2", update: { status: "running" } });
  });

  test("a completed turn with null text reads as no text, not as the string null", () => {
    const report = reportOf({ type: "turn.completed", sessionId: "s1", turnId: "t1", text: null });
    expect(report).toEqual({
      type: "turn.completed",
      sessionId: "s1",
      turnId: "t1",
      stopReason: "end_turn",
    });
  });
});

describe("a turn id that is present but not first", () => {
  test("only the named turn settles, even when another is live", () => {
    const effect = effectOf(
      { type: "turn.failed", sessionId: "s1", turnId: "t2", error: "boom" },
      session(),
      [turn({ id: "t1", status: "running" }), turn({ id: "t2", status: "running" })],
    );
    expect(effect).toEqual({
      kind: "settle",
      turnId: "t2",
      update: { status: "failed", error: "boom" },
    });
  });

  test("a turn with no text completes without a text field, not with an empty one", () => {
    const effect = effectOf(
      { type: "turn.completed", sessionId: "s1", turnId: "t1", stopReason: "end_turn" },
      session(),
      [turn({ status: "running" })],
    );
    expect(effect).not.toHaveProperty("text");
  });
});

describe("reading the capability an agent reported", () => {
  const report = (capability: unknown) =>
    reportOf({ type: "session.capability", sessionId: "s1", capability });

  test("the whole capability is read, including the agent's identity and current model", () => {
    expect(
      report({
        agent: { name: "OpenCode", version: "2.3.0" },
        currentModelId: "ollama-cloud/deepseek-v4.1-flash",
        availableModels: ["ollama-cloud/deepseek-v4.1-flash", "opencode/tidepool-alpha-free"],
        statesModels: true,
      }),
    ).toEqual({
      type: "session.capability",
      sessionId: "s1",
      capability: {
        agent: { name: "OpenCode", version: "2.3.0" },
        currentModelId: "ollama-cloud/deepseek-v4.1-flash",
        availableModels: ["ollama-cloud/deepseek-v4.1-flash", "opencode/tidepool-alpha-free"],
        statesModels: true,
      },
    });
  });

  test("a capability with no model list is not a report, so a bad frame cannot clear the record", () => {
    expect(report({ statesModels: true })).toBeUndefined();
    expect(report(undefined)).toBeUndefined();
    expect(report("models")).toBeUndefined();
    expect(report(null)).toBeUndefined();
  });

  test("a model entry that is not a string is dropped, keeping the real ones", () => {
    const read = report({ availableModels: ["a", 42, null, "b"], statesModels: true });
    expect(read).toMatchObject({ capability: { availableModels: ["a", "b"] } });
  });

  test("an identity that is not two strings is dropped rather than half-recorded", () => {
    for (const agent of [{ name: "x" }, { version: "1" }, { name: 1, version: "1" }, "x", null]) {
      expect(report({ agent, availableModels: [], statesModels: false })).toEqual({
        type: "session.capability",
        sessionId: "s1",
        capability: { availableModels: [], statesModels: false },
      });
    }
  });

  test("a current model that is not a string is left out", () => {
    expect(report({ currentModelId: 7, availableModels: [], statesModels: false })).toEqual({
      type: "session.capability",
      sessionId: "s1",
      capability: { availableModels: [], statesModels: false },
    });
    expect(report({ availableModels: [], statesModels: false })).not.toHaveProperty(
      "capability.currentModelId",
    );
  });

  test("only a stated true counts as naming models, so a missing field says nothing", () => {
    // A truthy-looking value that is not `true` must not be read as a list.
    const read = report({ availableModels: ["a"] });
    expect(read).toMatchObject({ capability: { statesModels: false } });
    expect(report({ availableModels: ["a"], statesModels: "yes" })).toMatchObject({
      capability: { statesModels: false },
    });
  });

  test("the capability is kept on the session, and it settles no turn", () => {
    expect(
      effectOf(
        {
          type: "session.capability",
          sessionId: "s1",
          capability: { availableModels: ["a"], statesModels: true },
        },
        session(),
        [],
      ),
    ).toEqual({ kind: "capability", capability: { availableModels: ["a"], statesModels: true } });
  });
});

describe("the capability reader's exact answers", () => {
  const report = (capability: unknown) =>
    reportOf({ type: "session.capability", sessionId: "s1", capability });

  test("a capability on a message that names no session is not a report", () => {
    expect(
      reportOf({
        type: "session.capability",
        capability: { availableModels: [], statesModels: false },
      }),
    ).toBeUndefined();
  });

  test("the whole record is pinned, so no field is added or lost", () => {
    expect(report({ availableModels: [], statesModels: false })).toEqual({
      type: "session.capability",
      sessionId: "s1",
      capability: { availableModels: [], statesModels: false },
    });
    // The identity and the current model are the only extra fields, and each is
    // present only when the agent stated it.
    const read = report({ availableModels: ["a"], statesModels: true });
    if (read?.type !== "session.capability") throw new Error("expected a capability report");
    expect(Object.keys(read.capability).sort()).toEqual(["availableModels", "statesModels"]);
  });

  test("a completion with no text settles with no text, because the agent said nothing", () => {
    // The reader must not invent an empty body for a completion that carried
    // none, or a turn with no words would look like an empty answer.
    const read = reportOf({ type: "turn.completed", sessionId: "s1", turnId: "t1" });
    expect(read).toEqual({
      type: "turn.completed",
      sessionId: "s1",
      turnId: "t1",
      stopReason: "end_turn",
    });
    expect(read).not.toHaveProperty("text");
  });
});
