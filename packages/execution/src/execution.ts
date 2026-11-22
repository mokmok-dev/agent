/**
 * The rules a durable execution follows. Cloudflare's durable Code Mode runtime
 * records every connector call and replays the finished ones, so a resumed run
 * never repeats an approved side effect. These are those rules as data, so the
 * same semantics run on Cloudflare and on a local VM.
 *
 * A pass runs the execution's source from the start and calls connectors in
 * order. The decision for call N reads the log at position N, so a loop that
 * calls one method twice still replays correctly.
 */

export const EXECUTION_STATUSES = ["running", "paused", "completed", "error", "rejected"] as const;

export type ExecutionStatus = (typeof EXECUTION_STATUSES)[number];

export const CALL_STATES = ["applied", "approved", "pending", "error"] as const;

/**
 * `applied` replays its result; `approved` runs on this pass because a person
 * allowed it; `pending` is waiting for that person; `error` ended the run.
 */
export type CallState = (typeof CALL_STATES)[number];

/** One connector call, at its position in the execution's log. */
export type CallRecord = {
  connector: string;
  method: string;
  args: unknown;
  state: CallState;
  /** `true` when the call must be approved before it runs the first time. */
  requiresApproval: boolean;
  /** The value a later pass replays instead of calling again. */
  result?: unknown;
  error?: string;
};

export type Execution = {
  id: string;
  code: string;
  status: ExecutionStatus;
  /** The call log, position N at index N - 1. */
  calls: readonly CallRecord[];
  result?: unknown;
  error?: string;
};

/** A call a pass is about to make. */
export type CallRequest = {
  connector: string;
  method: string;
  args: unknown;
  requiresApproval: boolean;
};

/**
 * What the runtime does with call `seq` of this pass. `replay` returns the
 * recorded result; `execute` lets the call run; `pause` stops until a person
 * approves position `seq`.
 */
export type CallDecision =
  | { kind: "replay"; result: unknown }
  | { kind: "execute"; seq: number }
  | { kind: "pause"; seq: number };

export function isTerminal(status: ExecutionStatus): boolean {
  return status === "completed" || status === "error" || status === "rejected";
}

/**
 * The decision for call `seq`. A recorded call replays its result, a pending
 * one pauses, and any other, including an approved or failed one, runs now.
 *
 * `seq` counts from 1 within the pass. The log can only grow by one at a time,
 * so a position past the end of the log plus one is a caller error.
 */
export function decide(execution: Execution, seq: number, call: CallRequest): CallDecision {
  if (seq < 1 || seq > execution.calls.length + 1) {
    throw new RangeError(`call ${seq} is outside the log of ${execution.calls.length}`);
  }
  const recorded = execution.calls[seq - 1];
  if (recorded !== undefined) {
    if (recorded.state === "applied") return { kind: "replay", result: recorded.result };
    if (recorded.state === "pending") return { kind: "pause", seq };
    return { kind: "execute", seq };
  }
  return call.requiresApproval ? { kind: "pause", seq } : { kind: "execute", seq };
}

/** Write one call at position `seq`, keeping every call before it. */
function write(execution: Execution, seq: number, record: CallRecord): readonly CallRecord[] {
  const calls = execution.calls.slice(0, seq - 1);
  calls.push(record);
  return calls;
}

/** The execution with call `seq` applied and its result recorded. */
export function recordCall(
  execution: Execution,
  seq: number,
  call: CallRequest,
  result: unknown,
): Execution {
  if (isTerminal(execution.status)) return execution;
  const record: CallRecord = { ...call, state: "applied", result };
  return { ...execution, status: "running", calls: write(execution, seq, record) };
}

/** The execution with call `seq` recorded as failed, which ends it. */
export function recordFailure(
  execution: Execution,
  seq: number,
  call: CallRequest,
  error: string,
): Execution {
  if (isTerminal(execution.status)) return execution;
  const record: CallRecord = { ...call, state: "error", error };
  return { ...execution, status: "error", error, calls: write(execution, seq, record) };
}

/** The execution paused at call `seq`, waiting for a person. */
export function pause(execution: Execution, seq: number, call: CallRequest): Execution {
  if (isTerminal(execution.status)) return execution;
  const record: CallRecord = { ...call, state: "pending" };
  return { ...execution, status: "paused", calls: write(execution, seq, record) };
}

/**
 * Allow the pending call and resume the pass. The call becomes `approved`, so
 * the resumed pass runs it once and records it; approving again changes
 * nothing, because no call is pending after the first approval.
 */
export function approve(execution: Execution): Execution {
  if (execution.status !== "paused") return execution;
  return {
    ...execution,
    status: "running",
    calls: execution.calls.map((record) =>
      record.state === "pending" ? { ...record, state: "approved" } : record,
    ),
  };
}

/**
 * Refuse the pending call, which ends the execution. Applied calls stay
 * applied, because refusing is not a rollback.
 */
export function reject(execution: Execution): Execution {
  if (execution.status !== "paused") return execution;
  return {
    ...execution,
    status: "rejected",
    calls: execution.calls.filter((record) => record.state !== "pending"),
  };
}

/** The execution completed with a result. */
export function complete(execution: Execution, result: unknown): Execution {
  if (isTerminal(execution.status)) return execution;
  return { ...execution, status: "completed", result };
}

/** The applied calls, newest first, which is the order a rollback walks. */
export function rollbackOrder(execution: Execution): CallRecord[] {
  return execution.calls.filter((record) => record.state === "applied").reverse();
}

export function pendingCalls(execution: Execution): CallRecord[] {
  return execution.calls.filter((record) => record.state === "pending");
}
