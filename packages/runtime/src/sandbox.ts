import type { AgentProcess, RuntimeTarget } from "./port.ts";
import { readLines } from "./process/lines.ts";

/**
 * The part of a Cloudflare Container a sandbox target needs. The platform's
 * `ctx.container` satisfies it; a test provides a fake.
 */
export interface SandboxContainer {
  readonly running: boolean;
  start(options: { image: string; entrypoint: string[]; enableInternet: boolean }): void;
  exec(command: string[]): Promise<SandboxProcess>;
}

export type SandboxProcess = {
  readonly stdout: ReadableStream<Uint8Array>;
  readonly stdin: { write(chunk: Uint8Array | string): Promise<void> | void } | null;
  readonly exited: Promise<number>;
  kill(signal?: string): void;
};

export type SandboxOptions = {
  container: SandboxContainer;
  /** The image the sandbox boots. The platform's Debian image holds Node and npm. */
  image?: string;
  /** Whether the sandbox can reach the internet. Off unless a tool needs it. */
  internet?: boolean;
};

export const DEFAULT_IMAGE = "cloudflare/debian-trixie";

/**
 * The Cloudflare sandbox target: the agent runs in a Container attached to the
 * Durable Object. The container starts on the first spawn and stays up between
 * turns, so a session keeps its files.
 *
 * Credentials stay outside the container: the agent calls a placeholder host,
 * and the Worker adds the real key after the request leaves.
 */
export function sandboxTarget(options: SandboxOptions): RuntimeTarget {
  return {
    runtime: "cloudflare-sandbox",
    spawn: (command, args) => sandboxProcess(options, [command, ...args]),
  };
}

/**
 * A container exec as an `AgentProcess`. Writes and exit listeners that arrive
 * before the exec resolves are held, so a caller can attach in any order. The
 * stdout read starts as soon as the exec resolves, so a line that arrives
 * before a listener attaches is held and delivered on attach.
 */
function sandboxProcess(options: SandboxOptions, argv: string[]): AgentProcess {
  const container = options.container;
  if (!container.running) {
    container.start({
      image: options.image ?? DEFAULT_IMAGE,
      entrypoint: ["sleep", "infinity"],
      enableInternet: options.internet ?? false,
    });
  }

  const heldWrites: string[] = [];
  const heldLines: string[] = [];
  const exits: ((code: number | null) => void)[] = [];
  const listeners: ((line: string) => void)[] = [];

  const deliver = (line: string): void => {
    if (listeners.length === 0) {
      heldLines.push(line);
      return;
    }
    for (const listener of listeners) listener(line);
  };

  const flushWrites = (process: SandboxProcess): void => {
    for (const held of heldWrites.splice(0)) void process.stdin?.write(held);
  };

  const ready = container.exec(argv);
  void ready.then((process) => {
    flushWrites(process);
    void process.exited.then((code) => {
      for (const listener of exits.splice(0)) listener(code);
    });
    void readLines(process.stdout, deliver);
  });

  return {
    stdin: {
      write: (chunk: string) => {
        heldWrites.push(chunk);
        void ready.then(flushWrites);
      },
    },
    stdout: {
      onLine: (listener) => {
        listeners.push(listener);
        for (const line of heldLines.splice(0)) listener(line);
      },
    },
    kill: () => {
      void ready.then((process) => process.kill());
    },
    once: (_event, listener) => {
      exits.push(listener);
    },
  };
}
