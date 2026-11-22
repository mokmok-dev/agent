#!/usr/bin/env bun
/**
 * Pi as an ACP agent.
 *
 * `pi --mode rpc` speaks JSONL commands and events. This entry point wires that
 * process to `PiAdapter`, which holds the protocol translation, and runs one pi
 * process per ACP session. It starts pi lazily, on `session/new`.
 */
import { LineFramer } from "@agent/runtime";
import { PiAdapter, piArgs, piConfigFrom, piEnv, type PiProcess } from "./pi-adapter.ts";

const config = piConfigFrom(process.env);
const args = piArgs(config);

function startPi(): PiProcess {
  const child = Bun.spawn(["pi", ...args], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: piEnv(process.env, config),
  });
  console.error(`pi adapter: started pi ${args.join(" ")}`);

  void (async () => {
    const decoder = new TextDecoder();
    for await (const chunk of child.stderr) process.stderr.write(`pi: ${decoder.decode(chunk)}`);
  })();

  return {
    send: (command) => {
      child.stdin.write(`${JSON.stringify(command)}\n`);
      void child.stdin.flush();
    },
    onLine: (listener) => {
      void (async () => {
        const framer = new LineFramer();
        for await (const chunk of child.stdout) {
          for (const line of framer.push(chunk)) listener(line);
        }
        const rest = framer.flush();
        if (rest !== undefined) listener(rest);
      })();
    },
    stop: () => child.kill(),
  };
}

const adapter = new PiAdapter({
  config,
  ports: {
    write: (frame) => {
      process.stdout.write(`${JSON.stringify(frame)}\n`);
    },
    startPi,
    commandId: () => `p-${Date.now()}`,
    timer: (ms, fire) => {
      const handle = setTimeout(fire, ms);
      return () => clearTimeout(handle);
    },
  },
});

const framer = new LineFramer();
for await (const chunk of Bun.stdin.stream()) {
  for (const line of framer.push(chunk)) adapter.handle(line);
}
const rest = framer.flush();
if (rest !== undefined) adapter.handle(rest);
adapter.closeAll();
