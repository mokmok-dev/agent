import { describe, expect, test } from "bun:test";
import { localVmTarget } from "./local-vm.ts";
import { RUNTIMES, type RuntimeTarget } from "./port.ts";
import { sandboxTarget, type SandboxContainer, type SandboxProcess } from "./sandbox.ts";

const AGENT = new URL("./process/line-agent.ts", import.meta.url).pathname;

/** The container contract, faked by running the argv as a real child. */
class FakeContainer implements SandboxContainer {
  running = false;
  started: { image: string; entrypoint: string[]; enableInternet: boolean } | undefined;
  execs: string[][] = [];

  start(options: { image: string; entrypoint: string[]; enableInternet: boolean }): void {
    this.started = options;
    this.running = true;
  }

  async exec(command: string[]): Promise<SandboxProcess> {
    this.execs.push(command);
    const child = Bun.spawn(command, {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "ignore",
      env: { ...process.env, AGENT_SCRIPT: "echo" },
    });
    return {
      stdout: child.stdout as ReadableStream<Uint8Array>,
      stdin: {
        write: (chunk: Uint8Array | string) => {
          child.stdin.write(chunk);
          void child.stdin.flush();
        },
      },
      exited: child.exited,
      kill: () => child.kill(),
    };
  }
}

/** One line in, the answering line out, for whichever target hosts the process. */
async function roundTrip(target: RuntimeTarget): Promise<string[]> {
  const process = target.spawn("bun", [AGENT]);
  const out: string[] = [];
  process.stdout?.onLine((line) => out.push(line));
  process.stdin?.write(`${JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize" })}\n`);
  for (let attempt = 0; attempt < 100 && out.length === 0; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  process.kill();
  return out;
}

describe("both targets satisfy the same contract", () => {
  test("each target names the runtime it hosts", () => {
    expect(localVmTarget().runtime).toBe("local-vm");
    expect(sandboxTarget({ container: new FakeContainer() }).runtime).toBe("cloudflare-sandbox");
    expect(RUNTIMES).toEqual(["cloudflare-sandbox", "local-vm"]);
  });

  test("the local VM target spawns a real child and answers on stdout", async () => {
    const out = await roundTrip(localVmTarget());
    expect(JSON.parse(out[0]!)).toMatchObject({ id: 1, result: { protocolVersion: 1 } });
  });

  test("the sandbox target answers the same frame from inside the container", async () => {
    const container = new FakeContainer();
    const out = await roundTrip(sandboxTarget({ container }));
    expect(JSON.parse(out[0]!)).toMatchObject({ id: 1, result: { protocolVersion: 1 } });
    expect(container.execs[0]).toEqual(["bun", AGENT]);
  });

  test("the sandbox target boots the container once, with the image and no internet", async () => {
    const container = new FakeContainer();
    await roundTrip(sandboxTarget({ container }));
    expect(container.started).toEqual({
      image: "cloudflare/debian-trixie",
      entrypoint: ["sleep", "infinity"],
      enableInternet: false,
    });
  });

  test("a booted container is not started again", async () => {
    const container = new FakeContainer();
    container.running = true;
    await roundTrip(sandboxTarget({ container }));
    expect(container.started).toBeUndefined();
  });

  test("an explicit image and internet access reach the container", async () => {
    const container = new FakeContainer();
    await roundTrip(sandboxTarget({ container, image: "cloudflare/python", internet: true }));
    expect(container.started).toMatchObject({ image: "cloudflare/python", enableInternet: true });
  });
});

describe("the sandbox process adapter", () => {
  test("stdin and stdout are present before the exec resolves", () => {
    const container = neverResolves();
    const spawned = sandboxTarget({ container }).spawn("agent", []);
    expect(spawned.stdin).not.toBeNull();
    expect(spawned.stdout).not.toBeNull();
    expect(() => spawned.stdin?.write("early\n")).not.toThrow();
    spawned.kill();
  });

  test("a write that arrives before the exec resolves is delivered when it does", async () => {
    const exec = new DeferredExec();
    const spawned = sandboxTarget({ container: exec.container }).spawn("agent", []);
    spawned.stdin?.write("first\n");
    expect(exec.writes).toEqual([]);
    exec.ready();
    await settle();
    expect(exec.writes).toEqual(["first\n"]);
  });

  test("a line that arrives before a listener is delivered on attach", async () => {
    const exec = new DeferredExec();
    const spawned = sandboxTarget({ container: exec.container }).spawn("agent", []);
    exec.ready();
    await settle();
    exec.emit("first");
    await settle();
    const lines: string[] = [];
    spawned.stdout?.onLine((line) => lines.push(line));
    expect(lines).toEqual(["first"]);
  });

  test("a listener that is attached first sees every line", async () => {
    const exec = new DeferredExec();
    const spawned = sandboxTarget({ container: exec.container }).spawn("agent", []);
    const lines: string[] = [];
    spawned.stdout?.onLine((line) => lines.push(line));
    exec.ready();
    await settle();
    exec.emit("first");
    exec.emit("second");
    await settle();
    expect(lines).toEqual(["first", "second"]);
  });

  test("kill reaches the exec once it starts", async () => {
    const exec = new DeferredExec();
    const spawned = sandboxTarget({ container: exec.container }).spawn("agent", []);
    spawned.kill();
    exec.ready();
    await settle();
    expect(exec.killed).toBe(true);
  });

  test("the exit code reaches a listener that attached first", async () => {
    const exec = new DeferredExec();
    const spawned = sandboxTarget({ container: exec.container }).spawn("agent", []);
    const codes: (number | null)[] = [];
    spawned.once("exit", (code) => codes.push(code));
    exec.ready();
    await settle();
    exec.exit(7);
    await settle();
    expect(codes).toEqual([7]);
  });
});

function settle(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 20));
}

/** A container whose exec never resolves, for the synchronous shape checks. */
function neverResolves(): SandboxContainer {
  return { running: true, start: () => undefined, exec: () => new Promise<never>(() => undefined) };
}

/** A container whose one exec is released and read by the test. */
class DeferredExec {
  writes: string[] = [];
  killed = false;
  #release: () => void = () => undefined;
  #push: ((chunk: Uint8Array) => void) | undefined;
  #end: (() => void) | undefined;
  #finish: ((code: number) => void) | undefined;

  readonly container: SandboxContainer = {
    running: true,
    start: () => undefined,
    exec: async () => {
      await new Promise<void>((resolve) => {
        this.#release = resolve;
      });
      const stdout = new ReadableStream<Uint8Array>({
        start: (controller) => {
          this.#push = (chunk) => controller.enqueue(chunk);
          this.#end = () => controller.close();
        },
      });
      return {
        stdout,
        stdin: { write: (chunk) => void this.writes.push(String(chunk)) },
        exited: new Promise<number>((resolve) => {
          this.#finish = resolve;
        }),
        kill: () => {
          this.killed = true;
        },
      };
    },
  };

  ready(): void {
    this.#release();
  }

  emit(line: string): void {
    this.#push?.(new TextEncoder().encode(`${line}\n`));
  }

  exit(code: number): void {
    this.#end?.();
    this.#finish?.(code);
  }
}
