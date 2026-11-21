import { describe, expect, test } from "bun:test";
import { spawnProcess } from "./proc.ts";

/** Wait for `predicate` to hold, so a real process gets time to run. */
async function eventually(predicate: () => boolean, attempts = 100): Promise<boolean> {
  for (let attempt = 0; attempt < attempts; attempt++) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return predicate();
}

describe("spawnProcess", () => {
  test("streams stdout lines from a real process", async () => {
    const lines: string[] = [];
    const child = spawnProcess("bun", ["-e", 'process.stdout.write("one\\ntwo\\n")']);
    child.stdout?.onLine((line) => lines.push(line));
    expect(await eventually(() => lines.length === 2)).toBe(true);
    expect(lines).toEqual(["one", "two"]);
  });

  test("writes what the child reads on stdin", async () => {
    const lines: string[] = [];
    const child = spawnProcess("cat", []);
    child.stdout?.onLine((line) => lines.push(line));
    child.stdin?.write("hello\n");
    expect(await eventually(() => lines.includes("hello"))).toBe(true);
    child.kill();
  });

  test("reports the exit code once the child exits", async () => {
    const codes: Array<number | null> = [];
    const child = spawnProcess("bun", ["-e", "process.exit(3)"]);
    child.once("exit", (code) => codes.push(code));
    expect(await eventually(() => codes.length === 1)).toBe(true);
    expect(codes).toEqual([3]);
  });

  test("kill stops a long-running child", async () => {
    const codes: Array<number | null> = [];
    const child = spawnProcess("bun", ["-e", "setTimeout(() => {}, 60_000)"]);
    child.once("exit", (code) => codes.push(code));
    child.kill();
    expect(await eventually(() => codes.length === 1)).toBe(true);
  });

  test("passes the environment through", async () => {
    const lines: string[] = [];
    const child = spawnProcess(
      "bun",
      ["-e", "process.stdout.write(process.env.PROBE ?? 'missing')"],
      {
        PROBE: "present",
      },
    );
    child.stdout?.onLine((line) => lines.push(line));
    expect(await eventually(() => lines.includes("present"))).toBe(true);
  });

  test("a non-UTF8 byte sequence still produces a line", async () => {
    const lines: string[] = [];
    const child = spawnProcess("bun", ["-e", "process.stdout.write(Buffer.from([0xff, 0x0a]))"]);
    child.stdout?.onLine((line) => lines.push(line));
    expect(await eventually(() => lines.length === 1)).toBe(true);
    expect(lines[0]!.length).toBeGreaterThan(0);
  });
});

describe("stdout that ends without a newline", () => {
  test("the trailing text is delivered as a line", async () => {
    const lines: string[] = [];
    const child = spawnProcess("bun", ["-e", 'process.stdout.write("no-newline")']);
    child.stdout?.onLine((line) => lines.push(line));
    expect(await eventually(() => lines.length === 1)).toBe(true);
    expect(lines).toEqual(["no-newline"]);
  });
});

describe("the kill binding", () => {
  test("killing a process that already exited is harmless", async () => {
    const codes: Array<number | null> = [];
    const child = spawnProcess("bun", ["-e", "process.exit(0)"]);
    child.once("exit", (code) => codes.push(code));
    expect(await eventually(() => codes.length === 1)).toBe(true);
    expect(() => child.kill()).not.toThrow();
  });
});

describe("a process that never exits", () => {
  test("kill ends it and its exit is reported", async () => {
    const codes: Array<number | null> = [];
    const child = spawnProcess("bun", ["-e", "setInterval(() => {}, 1000)"]);
    child.once("exit", (code) => codes.push(code));
    await new Promise((resolve) => setTimeout(resolve, 50));
    child.kill();
    expect(await eventually(() => codes.length === 1)).toBe(true);
  });
});

describe("the kill result", () => {
  test("kill returns undefined, the child signal being fire and forget", async () => {
    const child = spawnProcess("bun", ["-e", "setInterval(() => {}, 1000)"]);
    const result = (child as unknown as { kill(): unknown }).kill();
    expect(result).toBeUndefined();
  });
});
