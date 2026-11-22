#!/usr/bin/env bun
/**
 * Pi as an ACP agent.
 *
 * The gateway speaks ACP to every agent, so this adapter presents pi's own RPC
 * mode (JSONL commands and events, `pi --mode rpc`) as an ACP server: one
 * `initialize` and `session/new` per connection, and one `session/prompt` per
 * turn. Pi's text deltas become `agent_message_chunk` updates, and
 * `agent_settled` ends the turn.
 *
 * The two protocols differ in three ways this file settles:
 *
 * - Pi has no session id of its own contract; this adapter owns one and keeps
 *   one pi process per ACP session.
 * - Pi reports no final answer, only streaming deltas, so the adapter tracks
 *   whether text arrived and reports `end_turn` or `error`.
 * - Pi's `prompt` response only means the prompt was accepted, so the turn ends
 *   on `agent_settled`, which is pi's own "no work remains" signal.
 */
import { existsSync } from "node:fs";

const PROVIDER = process.env.PI_PROVIDER ?? "ollama-cloud";
const MODEL = process.env.PI_MODEL ?? "deepseek-v4.1-flash";
const AGENT_DIR = process.env.PI_CODING_AGENT_DIR;
const TURN_TIMEOUT_MS = Number(process.env.PI_TURN_TIMEOUT_MS ?? 240_000);

/** One pi subprocess and the turn it is answering, if any. */
type PiTurn = {
  sessionId: string;
  onDelta(text: string): void;
  settle(stopReason: string): void;
};

type PiProcess = {
  send(command: Record<string, unknown>): void;
  attach(turn: PiTurn): void;
  stop(): void;
};

/** Start pi in RPC mode and route its frames to the current turn. */
function startPi(): PiProcess {
  const args = ["--mode", "rpc", "--no-session", "--provider", PROVIDER, "--model", MODEL];
  const child = Bun.spawn(["pi", ...args], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    ...(AGENT_DIR === undefined ? {} : { env: { ...process.env, PI_CODING_AGENT_DIR: AGENT_DIR } }),
  });
  let turn: PiTurn | undefined;
  console.error(`pi adapter: started pi ${args.join(" ")}`);

  void (async () => {
    const decoder = new TextDecoder();
    let buffer = "";
    for await (const chunk of child.stdout) {
      buffer += decoder.decode(chunk, { stream: true });
      let newline = buffer.indexOf("\n");
      while (newline >= 0) {
        const line = buffer.slice(0, newline).replace(/\r$/, "");
        buffer = buffer.slice(newline + 1);
        if (line !== "") apply(line, () => turn);
        newline = buffer.indexOf("\n");
      }
    }
  })();

  void (async () => {
    const decoder = new TextDecoder();
    for await (const chunk of child.stderr) {
      process.stderr.write(`pi: ${decoder.decode(chunk)}`);
    }
  })();

  function apply(line: string, current: () => PiTurn | undefined): void {
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(line) as Record<string, unknown>;
    } catch {
      return;
    }
    const active = current();
    if (active === undefined) return;
    if (frame["type"] === "message_update") {
      const event = frame["assistantMessageEvent"] as
        { type?: string; delta?: unknown } | undefined;
      if (event?.type === "text_delta" && typeof event.delta === "string")
        active.onDelta(event.delta);
      return;
    }
    if (frame["type"] === "agent_settled") active.settle("end_turn");
  }

  return {
    send: (command) => {
      child.stdin.write(`${JSON.stringify(command)}\n`);
      void child.stdin.flush();
    },
    attach: (next) => {
      turn = next;
    },
    stop: () => child.kill(),
  };
}

/** Write one JSON-RPC frame, which is what the gateway reads. */
function write(frame: unknown): void {
  process.stdout.write(`${JSON.stringify(frame)}\n`);
}

function result(id: unknown, value: unknown): void {
  write({ jsonrpc: "2.0", id, result: value });
}

function failure(id: unknown, code: number, message: string): void {
  write({ jsonrpc: "2.0", id, error: { code, message } });
}

function update(sessionId: string, payload: unknown): void {
  write({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update: payload } });
}

/**
 * One ACP session over one pi process. Pi's RPC mode is stateful, so a session
 * is a process; a second session starts its own.
 */
class Session {
  readonly id: string;
  readonly #pi: PiProcess;

  constructor(id: string, pi: PiProcess) {
    this.id = id;
    this.#pi = pi;
  }

  prompt(text: string, done: (stopReason: string) => void): void {
    let sawText = false;
    const timer = setTimeout(() => done("cancelled"), TURN_TIMEOUT_MS);
    this.#pi.attach({
      sessionId: this.id,
      onDelta: (delta) => {
        sawText = true;
        update(this.id, {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: delta },
        });
      },
      settle: (stopReason) => {
        clearTimeout(timer);
        done(sawText ? stopReason : "error");
      },
    });
    this.#pi.send({ id: `p-${Date.now()}`, type: "prompt", message: text });
  }

  close(): void {
    this.#pi.stop();
  }
}

const sessions = new Map<string, Session>();
let counter = 0;

/** Read stdin as JSONL, which is strict one-record-per-line framing. */
async function main(): Promise<void> {
  const decoder = new TextDecoder();
  let buffer = "";
  for await (const chunk of Bun.stdin.stream()) {
    buffer += decoder.decode(chunk, { stream: true });
    let newline = buffer.indexOf("\n");
    while (newline >= 0) {
      const line = buffer.slice(0, newline).replace(/\r$/, "");
      buffer = buffer.slice(newline + 1);
      if (line !== "") handle(line);
      newline = buffer.indexOf("\n");
    }
  }
  for (const session of sessions.values()) session.close();
}

function handle(line: string): void {
  let frame: Record<string, unknown>;
  try {
    frame = JSON.parse(line) as Record<string, unknown>;
  } catch {
    failure(null, -32700, "pi adapter could not parse a frame");
    return;
  }
  const id = frame["id"];
  const method = frame["method"];
  const params = (frame["params"] ?? {}) as Record<string, unknown>;
  console.error(`pi adapter: ${String(method)}`);

  if (method === "initialize") {
    result(id, {
      protocolVersion: 1,
      agentCapabilities: { promptCapabilities: { embeddedContext: true, image: false } },
    });
    return;
  }
  if (method === "session/new") {
    counter += 1;
    const session = new Session(`pi-${counter}`, startPi());
    sessions.set(session.id, session);
    result(id, { sessionId: session.id });
    return;
  }
  if (method === "session/prompt") {
    const sessionId = String(params["sessionId"] ?? "");
    const session = sessions.get(sessionId);
    if (session === undefined) {
      failure(id, -32602, `no pi session ${sessionId}`);
      return;
    }
    const text = promptText(params["prompt"]);
    session.prompt(text, (stopReason) => result(id, { stopReason }));
    return;
  }
  if (method === "session/cancel") {
    // pi's own cancel is a separate command; ending the process is the
    // adapter's clean way to abandon a turn.
    const sessionId = String(params["sessionId"] ?? "");
    sessions.get(sessionId)?.close();
    sessions.delete(sessionId);
    return;
  }
  failure(id, -32601, `pi adapter does not implement ${String(method)}`);
}

/** The text of an ACP prompt, which is a list of content blocks. */
function promptText(prompt: unknown): string {
  if (!Array.isArray(prompt)) return "";
  const parts: string[] = [];
  for (const block of prompt) {
    const typed = block as { type?: string; text?: unknown };
    if (typed.type === "text" && typeof typed.text === "string") parts.push(typed.text);
  }
  return parts.join("\n");
}

if (existsSync("/dev/stdin")) void main();
