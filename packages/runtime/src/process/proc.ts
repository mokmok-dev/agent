import { readLines } from "./lines.ts";
import type { ChildProcess, Spawn } from "./supervisor.ts";

export function spawnProcess(
  command: string,
  args: readonly string[],
  env: Record<string, string | undefined> = {},
): ChildProcess {
  const child = Bun.spawn([command, ...args], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, ...env },
  });
  const stdout = child.stdout as ReadableStream<Uint8Array>;
  return {
    stdin: {
      write: (chunk: string) => {
        child.stdin.write(chunk);
        void child.stdin.flush();
      },
    },
    stdout: {
      onLine: (listener) => {
        void readLines(stdout, listener);
      },
    },
    kill: () => {
      child.kill();
    },
    once: (_event, listener) => {
      void child.exited.then((code) => listener(code));
    },
  };
}

export const spawnReal: Spawn = spawnProcess;
