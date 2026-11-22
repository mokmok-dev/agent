/**
 * Where an agent process runs. A session is bound to one of these for its
 * whole life, so a turn never has to ask again, and the two implementations
 * differ only in where the bytes go.
 */

export const RUNTIMES = ["cloudflare-sandbox", "local-vm"] as const;

export type Runtime = (typeof RUNTIMES)[number];

/** One child process, as both a local spawn and a sandbox exec present it. */
export interface AgentProcess {
  readonly stdin: { write(chunk: string): void } | null;
  readonly stdout: { onLine(listener: (line: string) => void): void } | null;
  kill(signal?: string): void;
  once(event: "exit", listener: (code: number | null) => void): void;
}

/**
 * Starts one agent process. A local target spawns a child of the gateway; a
 * sandbox target runs the same command inside the container attached to the
 * Durable Object. Everything above this line is identical for both.
 */
export type Spawn = (command: string, args: readonly string[]) => AgentProcess;

/**
 * The port a runtime target fills. `start` is the only entry point a gateway
 * needs, because the supervisor owns the process after that.
 */
export type RuntimeTarget = {
  runtime: Runtime;
  /** Start the agent process this target hosts. */
  spawn: Spawn;
};
