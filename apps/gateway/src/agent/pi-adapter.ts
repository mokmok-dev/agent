/**
 * Pi's RPC mode as an ACP server.
 *
 * The gateway speaks ACP to every agent, so this adapter translates in both
 * directions: ACP requests become pi RPC commands, and pi's JSONL events become
 * ACP results and `session/update` notifications. The process handling lives in
 * `pi-acp.ts`; everything here is a pure decision over injected ports, so the
 * translation is tested without a pi binary.
 *
 * The two protocols differ in three ways this module settles:
 *
 * - Pi has no session id of its own contract; the adapter owns one and keeps
 *   one pi process per ACP session.
 * - Pi reports no final answer, only streaming deltas, so the adapter tracks
 *   whether text arrived and reports `end_turn` or `error`.
 * - Pi's `prompt` response only means the prompt was accepted, so the turn ends
 *   on `agent_settled`, which is pi's own "no work remains" signal.
 */

/** One pi subprocess: its commands, its lines, and its end. */
export type PiProcess = {
  send(command: Record<string, unknown>): void;
  /** Hand every complete line pi writes to `listener`. */
  onLine(listener: (line: string) => void): void;
  stop(): void;
};

/** What the adapter needs from the host, so tests drive it without a process. */
export type PiPorts = {
  /** Write one JSON-RPC frame to the gateway. */
  write(frame: unknown): void;
  /** Start one pi process per ACP session. */
  startPi(): PiProcess;
  /** A command id for pi, which must differ between prompts. */
  commandId(): string;
  /** Run `fire` after `ms`; the returned function cancels it. */
  timer(ms: number, fire: () => void): () => void;
};

export type PiConfig = {
  provider: string;
  model: string;
  agentDir: string | undefined;
  turnTimeoutMs: number;
};

const DEFAULT_PROVIDER = "ollama-cloud";
const DEFAULT_MODEL = "deepseek-v4.1-flash";
const DEFAULT_TURN_TIMEOUT_MS = 240_000;

/** Resolve the pi settings from the environment, with the shipped defaults. */
export function piConfigFrom(env: Record<string, string | undefined>): PiConfig {
  return {
    provider: env["PI_PROVIDER"] ?? DEFAULT_PROVIDER,
    model: env["PI_MODEL"] ?? DEFAULT_MODEL,
    agentDir: env["PI_CODING_AGENT_DIR"],
    turnTimeoutMs: Number(env["PI_TURN_TIMEOUT_MS"] ?? DEFAULT_TURN_TIMEOUT_MS),
  };
}

/** The arguments that put pi into the RPC mode this adapter translates. */
export function piArgs(config: PiConfig): string[] {
  return ["--mode", "rpc", "--no-session", "--provider", config.provider, "--model", config.model];
}

/** The child environment, which needs pi's agent directory when stated. */
export function piEnv(
  base: Record<string, string | undefined>,
  config: PiConfig,
): Record<string, string | undefined> {
  if (config.agentDir === undefined) return base;
  return { ...base, PI_CODING_AGENT_DIR: config.agentDir };
}

/** The text of an ACP prompt, which is a list of content blocks. */
export function promptText(prompt: unknown): string {
  if (!Array.isArray(prompt)) return "";
  const parts: string[] = [];
  for (const block of prompt) {
    const typed = block as { type?: string; text?: unknown };
    if (typed.type === "text" && typeof typed.text === "string") parts.push(typed.text);
  }
  return parts.join("\n");
}

/**
 * The reply pi's event stream implies. A settled turn that produced no text is
 * a failure the gateway can report, rather than an empty answer.
 */
export function stopReasonFor(sawText: boolean, settled: string): string {
  return sawText ? settled : "error";
}

/** The text a pi event frame carries, if it is the delta this adapter forwards. */
export function deltaOf(frame: Record<string, unknown>): string | undefined {
  if (frame["type"] !== "message_update") return undefined;
  const event = frame["assistantMessageEvent"] as { type?: string; delta?: unknown } | undefined;
  if (event?.type !== "text_delta") return undefined;
  return typeof event.delta === "string" ? event.delta : undefined;
}

/** The stop reason a pi frame implies, when it ends a turn. */
export function settleOf(frame: Record<string, unknown>): string | undefined {
  return frame["type"] === "agent_settled" ? "end_turn" : undefined;
}

function writeResult(ports: PiPorts, id: unknown, value: unknown): void {
  ports.write({ jsonrpc: "2.0", id, result: value });
}

function writeFailure(ports: PiPorts, id: unknown, code: number, message: string): void {
  ports.write({ jsonrpc: "2.0", id, error: { code, message } });
}

function update(ports: PiPorts, sessionId: string, payload: unknown): void {
  ports.write({
    jsonrpc: "2.0",
    method: "session/update",
    params: { sessionId, update: payload },
  });
}

/** One ACP session over one pi process. Pi's RPC mode is stateful, so a session is a process. */
class Session {
  readonly id: string;
  readonly #pi: PiProcess;
  readonly #ports: PiPorts;
  readonly #timeoutMs: number;
  #turn: { onDelta(text: string): void; settle(stopReason: string): void } | undefined;

  constructor(id: string, pi: PiProcess, ports: PiPorts, timeoutMs: number) {
    this.id = id;
    this.#pi = pi;
    this.#ports = ports;
    this.#timeoutMs = timeoutMs;
  }

  prompt(text: string, done: (stopReason: string) => void): void {
    let sawText = false;
    let settled = false;
    let cancel: () => void = () => {};
    // A timeout abandons the turn, so it reports `cancelled` whether or not
    // text arrived; only pi's own settle is judged by whether text arrived.
    const finish = (stopReason: string): void => {
      if (settled) return;
      settled = true;
      cancel();
      done(stopReason);
    };
    cancel = this.#ports.timer(this.#timeoutMs, () => finish("cancelled"));
    this.#turn = {
      onDelta: (delta) => {
        sawText = true;
        update(this.#ports, this.id, {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: delta },
        });
      },
      settle: (stopReason) => finish(stopReasonFor(sawText, stopReason)),
    };
    this.#pi.send({ id: this.#ports.commandId(), type: "prompt", message: text });
  }

  /**
   * Handle one line pi wrote for this session. A frame that is not JSON, or
   * that arrives with no turn in flight, is dropped: pi writes startup chatter
   * this adapter ignores.
   */
  receive(line: string): void {
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(line) as Record<string, unknown>;
    } catch {
      return;
    }
    const turn = this.#turn;
    if (turn === undefined) return;
    const delta = deltaOf(frame);
    if (delta !== undefined) {
      turn.onDelta(delta);
      return;
    }
    const settled = settleOf(frame);
    if (settled !== undefined) turn.settle(settled);
  }

  close(): void {
    this.#pi.stop();
  }
}

export class PiAdapter {
  readonly #ports: PiPorts;
  readonly #config: PiConfig;
  readonly #sessions = new Map<string, Session>();
  #counter = 0;

  constructor(options: { ports: PiPorts; config: PiConfig }) {
    this.#ports = options.ports;
    this.#config = options.config;
  }

  /** The live ACP sessions, which the entry point reads on shutdown. */
  sessionIds(): string[] {
    return [...this.#sessions.keys()];
  }

  /** Handle one ACP request line from the gateway. */
  handle(line: string): void {
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(line) as Record<string, unknown>;
    } catch {
      writeFailure(this.#ports, null, -32700, "pi adapter could not parse a frame");
      return;
    }
    const id = frame["id"];
    const method = frame["method"];
    const params = (frame["params"] ?? {}) as Record<string, unknown>;

    if (method === "initialize") {
      writeResult(this.#ports, id, {
        protocolVersion: 1,
        agentCapabilities: { promptCapabilities: { embeddedContext: true, image: false } },
        // pi is not an ACP agent, so it has no name or version of its own to
        // report here; the adapter is what speaks this protocol.
        agentInfo: { name: "pi-adapter", version: "0" },
      });
      return;
    }
    if (method === "session/new") {
      this.#counter += 1;
      const sessionId = `pi-${this.#counter}`;
      const pi = this.#ports.startPi();
      const session = new Session(sessionId, pi, this.#ports, this.#config.turnTimeoutMs);
      // Each process's lines belong to its own session, so two sessions sharing
      // the adapter cannot read each other's deltas.
      pi.onLine((line) => session.receive(line));
      this.#sessions.set(sessionId, session);
      // pi's model is fixed by the command line, so the one it can run is the
      // one it was started with. Reporting it lets the control plane see that
      // the route's model is the one this agent actually uses.
      writeResult(this.#ports, id, {
        sessionId,
        models: {
          currentModelId: this.#config.model,
          availableModels: [{ modelId: this.#config.model, name: this.#config.model }],
        },
      });
      return;
    }
    if (method === "session/prompt") {
      const sessionId = String(params["sessionId"] ?? "");
      const session = this.#sessions.get(sessionId);
      if (session === undefined) {
        writeFailure(this.#ports, id, -32602, `no pi session ${sessionId}`);
        return;
      }
      session.prompt(promptText(params["prompt"]), (stopReason) =>
        writeResult(this.#ports, id, { stopReason }),
      );
      return;
    }
    if (method === "session/cancel") {
      // pi's own cancel is a separate command; ending the process is the
      // adapter's clean way to abandon a turn.
      const sessionId = String(params["sessionId"] ?? "");
      this.#sessions.get(sessionId)?.close();
      this.#sessions.delete(sessionId);
      return;
    }
    writeFailure(this.#ports, id, -32601, `pi adapter does not implement ${String(method)}`);
  }

  /** Close every session, which the entry point does when stdin ends. */
  closeAll(): void {
    for (const session of this.#sessions.values()) session.close();
    this.#sessions.clear();
  }
}
