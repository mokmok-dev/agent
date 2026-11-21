/** A child process, as the supervisor needs it. */
export interface ChildProcess {
  readonly stdin: { write(chunk: string): void } | null;
  readonly stdout: { onLine(listener: (line: string) => void): void } | null;
  kill(signal?: string): void;
  once(event: "exit", listener: (code: number | null) => void): void;
}

/** How a process is started. A test supplies a fake. */
export type Spawn = (command: string, args: readonly string[]) => ChildProcess;

export type SupervisorState = "idle" | "running" | "stopped";

type Handlers = {
  onLine: (line: string) => void;
  onExit?: (code: number | null, restarts: number) => void;
};

/** A command to run, fixed at `start` so every restart reuses it. */
type Launch = { command: string; args: readonly string[] };

/**
 * Runs one agent process for one session. A crashed process is restarted at
 * most `maxRestarts` times; after that the session is failed rather than
 * restarting a process that keeps dying.
 */
export class AgentSupervisor {
  readonly #spawn: Spawn;
  readonly #maxRestarts: number;
  /** The command a restart reuses. Undefined until `start`. */
  #launch: Launch | undefined;
  #handlers!: Handlers;
  #child: ChildProcess | undefined;
  #restarts = 0;
  #state: SupervisorState = "idle";

  constructor(options: { spawn: Spawn; maxRestarts?: number }) {
    this.#spawn = options.spawn;
    this.#maxRestarts = options.maxRestarts ?? 3;
  }

  get state(): SupervisorState {
    return this.#state;
  }

  get restarts(): number {
    return this.#restarts;
  }

  /** Start the process and stream its stdout lines. */
  start(command: string, args: readonly string[], handlers: Handlers): void {
    this.#launch = { command, args };
    this.#handlers = handlers;
    this.#restarts = 0;
    this.#spawnOnce();
  }

  /** Write one ACP frame to the process. A stopped process drops the write. */
  write(line: string): boolean {
    const stdin = this.#child?.stdin;
    if (stdin == null) return false;
    stdin.write(`${line}\n`);
    return true;
  }

  /** Stop the process. A later exit does not restart it. */
  stop(): void {
    this.#state = "stopped";
    this.#child?.kill();
    this.#child = undefined;
  }

  #spawnOnce(): void {
    this.#state = "running";
    const { command, args } = this.#launch!;
    const child = this.#spawn(command, args);
    this.#child = child;
    child.stdout?.onLine((line) => this.#handlers.onLine(line));
    child.once("exit", (code) => this.#handleExit(code));
  }

  /** One restart per exit until the limit. The code is the one the process exited with. */
  #handleExit(code: number | null): void {
    const exitedAfterStop = this.#state === "stopped";
    const outOfRestarts = this.#restarts === this.#maxRestarts;
    if (exitedAfterStop) return;
    if (!outOfRestarts) {
      this.#restarts += 1;
      this.#spawnOnce();
      return;
    }
    this.#state = "stopped";
    this.#child = undefined;
    this.#handlers.onExit?.(code, this.#restarts);
  }
}
