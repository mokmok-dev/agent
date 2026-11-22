import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  approve,
  CALL_STATES,
  complete,
  decide,
  EXECUTION_STATUSES,
  isTerminal,
  pause,
  pendingCalls,
  recordCall,
  recordFailure,
  reject,
  rollbackOrder,
  type CallRequest,
  type Execution,
} from "./execution.ts";

const execution = (overrides: Partial<Execution> = {}): Execution => ({
  id: "e1",
  code: "async () => 1",
  status: "running",
  calls: [],
  ...overrides,
});

const call = (overrides: Partial<CallRequest> = {}): CallRequest => ({
  connector: "notes",
  method: "listNotes",
  args: {},
  requiresApproval: false,
  ...overrides,
});

describe("deciding the next call", () => {
  test("an unrecorded call without approval executes", () => {
    expect(decide(execution(), 1, call())).toEqual({ kind: "execute", seq: 1 });
  });

  test("an unrecorded call with approval pauses first", () => {
    expect(decide(execution(), 1, call({ requiresApproval: true }))).toEqual({
      kind: "pause",
      seq: 1,
    });
  });

  test("an applied call replays its recorded result", () => {
    const first = recordCall(execution(), 1, call(), { notes: ["a"] });
    expect(decide(first, 1, call())).toEqual({ kind: "replay", result: { notes: ["a"] } });
  });

  test("an approved call executes rather than pausing again", () => {
    const paused = pause(execution(), 1, call({ requiresApproval: true }));
    const resumed = approve(paused);
    expect(decide(resumed, 1, call({ requiresApproval: true }))).toEqual({
      kind: "execute",
      seq: 1,
    });
  });

  test("a position past the end of the log is a caller error", () => {
    expect(() => decide(execution(), 2, call())).toThrow(RangeError);
    expect(() => decide(execution(), 0, call())).toThrow(RangeError);
  });

  test("a failed call is retried rather than replayed", () => {
    const failed = recordFailure(execution(), 1, call(), "boom");
    expect(decide(failed, 1, call())).toEqual({ kind: "execute", seq: 1 });
  });

  test("a call already waiting for approval still pauses, so a retry is stable", () => {
    const paused = pause(execution(), 1, call({ requiresApproval: true }));
    expect(decide(paused, 1, call({ requiresApproval: true }))).toEqual({ kind: "pause", seq: 1 });
  });

  test("a call that no longer needs approval runs once it is approved", () => {
    const paused = pause(execution(), 1, call({ requiresApproval: true }));
    expect(decide(approve(paused), 1, call({ requiresApproval: false }))).toEqual({
      kind: "execute",
      seq: 1,
    });
  });

  test("a recorded call replays even when a later pass asks for approval", () => {
    const applied = recordCall(execution(), 1, call(), "seen");
    expect(decide(applied, 1, call({ requiresApproval: true }))).toEqual({
      kind: "replay",
      result: "seen",
    });
  });
});

describe("recording a call", () => {
  test("applies the call and its result in order", () => {
    const first = recordCall(execution(), 1, call({ method: "a" }), 1);
    const second = recordCall(first, 2, call({ method: "b" }), 2);
    expect(second.calls.map((record) => [record.method, record.state, record.result])).toEqual([
      ["a", "applied", 1],
      ["b", "applied", 2],
    ]);
  });

  test("the same method twice keeps both calls, keyed by position", () => {
    const first = recordCall(execution(), 1, call({ args: { n: 1 } }), "one");
    const second = recordCall(first, 2, call({ args: { n: 2 } }), "two");
    expect(decide(second, 1, call())).toEqual({ kind: "replay", result: "one" });
    expect(decide(second, 2, call())).toEqual({ kind: "replay", result: "two" });
  });

  test("a record after a pause rewrites the pending position, not a new one", () => {
    const paused = pause(execution(), 1, call({ requiresApproval: true }));
    const applied = recordCall(approve(paused), 1, call({ requiresApproval: true }), "done");
    expect(applied.calls).toHaveLength(1);
    expect(applied.calls[0]?.state).toBe("applied");
  });

  test("a failure ends the execution and keeps the error", () => {
    const failed = recordFailure(execution(), 1, call(), "network down");
    expect(failed.status).toBe("error");
    expect(failed.error).toBe("network down");
    expect(failed.calls[0]).toMatchObject({ state: "error", error: "network down" });
  });

  test("recording a call returns a paused execution to running", () => {
    const paused = approve(pause(execution(), 1, call({ requiresApproval: true })));
    expect(paused.status).toBe("running");
    const applied = recordCall({ ...paused, status: "paused" }, 1, call(), "done");
    expect(applied.status).toBe("running");
  });

  test("recording a call keeps the connector, method, and args it ran with", () => {
    const request = call({ connector: "github", method: "createIssue", args: { title: "T" } });
    expect(recordCall(execution(), 1, request, "ok").calls[0]).toEqual({
      connector: "github",
      method: "createIssue",
      args: { title: "T" },
      requiresApproval: false,
      state: "applied",
      result: "ok",
    });
  });
});

describe("approval", () => {
  test("pausing records a pending call and stops the run", () => {
    const paused = pause(execution(), 1, call({ requiresApproval: true }));
    expect(paused.status).toBe("paused");
    expect(pendingCalls(paused)).toHaveLength(1);
  });

  test("approving resumes and marks the call approved", () => {
    const resumed = approve(pause(execution(), 1, call({ requiresApproval: true })));
    expect(resumed.status).toBe("running");
    expect(resumed.calls[0]?.state).toBe("approved");
    expect(pendingCalls(resumed)).toEqual([]);
  });

  test("approving a second time changes nothing", () => {
    const resumed = approve(pause(execution(), 1, call({ requiresApproval: true })));
    expect(approve(resumed)).toEqual(resumed);
  });

  test("rejecting ends the execution and drops the pending call", () => {
    const rejected = reject(pause(execution(), 1, call({ requiresApproval: true })));
    expect(rejected.status).toBe("rejected");
    expect(rejected.calls).toEqual([]);
  });

  test("rejecting keeps the calls that already applied", () => {
    const first = recordCall(execution(), 1, call({ method: "read" }), "ok");
    const rejected = reject(pause(first, 2, call({ method: "write", requiresApproval: true })));
    expect(rejected.calls.map((record) => record.method)).toEqual(["read"]);
  });

  test("approving and rejecting a running execution do nothing", () => {
    const running = execution();
    expect(approve(running)).toBe(running);
    expect(reject(running)).toBe(running);
  });
});

describe("terminal executions", () => {
  test("every write leaves a terminal execution alone", () => {
    for (const status of ["completed", "error", "rejected"] as const) {
      const done = execution({ status });
      expect(recordCall(done, 1, call(), 1)).toBe(done);
      expect(recordFailure(done, 1, call(), "x")).toBe(done);
      expect(pause(done, 1, call())).toBe(done);
      expect(complete(done, "r")).toBe(done);
    }
  });

  test("isTerminal agrees with the status list", () => {
    expect(isTerminal("running")).toBe(false);
    expect(isTerminal("paused")).toBe(false);
    expect(isTerminal("completed")).toBe(true);
    expect(isTerminal("error")).toBe(true);
    expect(isTerminal("rejected")).toBe(true);
  });

  test("the status lists name every state the rules use", () => {
    expect(EXECUTION_STATUSES).toEqual(["running", "paused", "completed", "error", "rejected"]);
    expect(CALL_STATES).toEqual(["applied", "approved", "pending", "error"]);
  });

  test("a failure record names the call it failed", () => {
    const request = call({ method: "send", requiresApproval: true });
    expect(recordFailure(execution(), 1, request, "denied").calls[0]).toEqual({
      connector: "notes",
      method: "send",
      args: {},
      requiresApproval: true,
      state: "error",
      error: "denied",
    });
  });

  test("a pending record keeps the approval flag that made it wait", () => {
    const paused = pause(execution(), 1, call({ requiresApproval: true }));
    expect(paused.calls[0]).toMatchObject({ state: "pending", requiresApproval: true });
  });

  test("the range error names the call and the log length", () => {
    expect(() => decide(execution(), 4, call())).toThrow("call 4 is outside the log of 0");
  });

  test("completing records the result and a second completion keeps the first", () => {
    const done = complete(execution(), "first");
    expect(done).toMatchObject({ status: "completed", result: "first" });
    expect(complete(done, "second").result).toBe("first");
  });
});

describe("rollback order", () => {
  test("returns applied calls newest first, so a rollback walks back", () => {
    const first = recordCall(execution(), 1, call({ method: "a" }), 1);
    const second = recordCall(first, 2, call({ method: "b" }), 2);
    expect(rollbackOrder(second).map((record) => record.method)).toEqual(["b", "a"]);
  });

  test("skips a pending call and an approved call that has not run", () => {
    const first = recordCall(execution(), 1, call({ method: "a" }), 1);
    const paused = pause(first, 2, call({ method: "b", requiresApproval: true }));
    expect(rollbackOrder(paused).map((record) => record.method)).toEqual(["a"]);
    expect(rollbackOrder(approve(paused)).map((record) => record.method)).toEqual(["a"]);
  });
});

describe("approved work runs exactly once", () => {
  test("over any approve/replay sequence, each call is applied at most once", () => {
    const anyCall = fc.record({
      method: fc.constantFrom("read", "write", "delete"),
      requiresApproval: fc.boolean(),
    });
    fc.assert(
      fc.property(
        fc.array(anyCall, { minLength: 1, maxLength: 6 }),
        fc.array(fc.boolean(), { maxLength: 6 }),
        (plan, approvals) => {
          let current = execution();
          let applied = 0;
          for (const [index, step] of plan.entries()) {
            const request = call({ method: step.method, requiresApproval: step.requiresApproval });
            const seq = index + 1;
            if (decide(current, seq, request).kind === "pause") {
              current = pause(current, seq, request);
              if (approvals[index] === false) {
                current = reject(current);
                break;
              }
              current = approve(current);
            }
            const decision = decide(current, seq, request);
            if (decision.kind === "execute") {
              current = recordCall(current, seq, request, seq);
              applied += 1;
            }
            expect(decide(current, seq, request).kind).toBe("replay");
          }
          expect(applied).toBe(current.calls.filter((r) => r.state === "applied").length);
        },
      ),
    );
  });

  test("a log never holds two records at one position", () => {
    fc.assert(
      fc.property(fc.array(fc.boolean(), { minLength: 1, maxLength: 8 }), (calls) => {
        let current = execution();
        for (const [index, requiresApproval] of calls.entries()) {
          const seq = index + 1;
          const request = call({ method: `m${seq}`, requiresApproval });
          if (decide(current, seq, request).kind === "pause")
            current = approve(pause(current, seq, request));
          current = recordCall(current, seq, request, seq);
        }
        expect(current.calls).toHaveLength(calls.length);
        expect(new Set(current.calls.map((r) => r.method)).size).toBe(calls.length);
      }),
    );
  });
});
