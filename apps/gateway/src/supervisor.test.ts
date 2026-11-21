import { describe, expect, test } from "bun:test";
import { AgentSupervisor, type ChildProcess } from "./supervisor.ts";

class FakeChild implements ChildProcess {
  readonly written: string[] = [];
  readonly stdin = { write: (chunk: string) => this.written.push(chunk) };
  readonly stdout = { onLine: (listener: (line: string) => void) => (this.listen = listener) };
  killed = false;
  listen: ((line: string) => void) | undefined;
  readonly exits: ((code: number | null) => void)[] = [];

  kill(): void {
    this.killed = true;
  }

  once(event: "exit", listener: (code: number | null) => void): void {
    if (event !== "exit") return;
    this.exits.push(listener);
  }

  exit(code: number | null): void {
    for (const listener of this.exits.splice(0)) listener(code);
  }

  emit(line: string): void {
    this.listen?.(line);
  }
}

const supervisor = (maxRestarts?: number) => {
  const children: FakeChild[] = [];
  const spawn = () => {
    const child = new FakeChild();
    children.push(child);
    return child;
  };
  return {
    children,
    supervisor: new AgentSupervisor({
      spawn,
      ...(maxRestarts === undefined ? {} : { maxRestarts }),
    }),
  };
};

describe("start", () => {
  test("spawns one process and reports running", () => {
    const { children, supervisor: sup } = supervisor();
    sup.start("agent", ["--acp"], { onLine: () => {} });
    expect(children).toHaveLength(1);
    expect(sup.state).toBe("running");
    expect(sup.restarts).toBe(0);
  });

  test("stdout lines reach the handler", () => {
    const { children, supervisor: sup } = supervisor();
    const lines: string[] = [];
    sup.start("agent", [], { onLine: (line) => lines.push(line) });
    children[0]!.emit("one");
    children[0]!.emit("two");
    expect(lines).toEqual(["one", "two"]);
  });
});

describe("write", () => {
  test("a running process receives the frame with a newline", () => {
    const { children, supervisor: sup } = supervisor();
    sup.start("agent", [], { onLine: () => {} });
    expect(sup.write("frame")).toBe(true);
    expect(children[0]!.written).toEqual(["frame\n"]);
  });

  test("an idle supervisor refuses the write", () => {
    const { supervisor: sup } = supervisor();
    expect(sup.write("frame")).toBe(false);
  });

  test("a stopped supervisor refuses the write", () => {
    const { supervisor: sup } = supervisor();
    sup.start("agent", [], { onLine: () => {} });
    sup.stop();
    expect(sup.write("frame")).toBe(false);
  });
});

describe("stop", () => {
  test("kills the child and reports stopped", () => {
    const { children, supervisor: sup } = supervisor();
    sup.start("agent", [], { onLine: () => {} });
    sup.stop();
    expect(children[0]!.killed).toBe(true);
    expect(sup.state).toBe("stopped");
  });

  test("an exit after stop does not restart", () => {
    const { children, supervisor: sup } = supervisor();
    sup.start("agent", [], { onLine: () => {} });
    sup.stop();
    children[0]!.exit(0);
    expect(children).toHaveLength(1);
  });
});

describe("restart", () => {
  test("a crash before the limit restarts the process", () => {
    const { children, supervisor: sup } = supervisor(2);
    sup.start("agent", [], { onLine: () => {} });
    children[0]!.exit(1);
    expect(children).toHaveLength(2);
    expect(sup.restarts).toBe(1);
    expect(sup.state).toBe("running");
  });

  test("a crash at the limit stops and reports the exit", () => {
    const { children, supervisor: sup } = supervisor(1);
    const exits: Array<[number | null, number]> = [];
    sup.start("agent", [], {
      onLine: () => {},
      onExit: (code, restarts) => exits.push([code, restarts]),
    });
    children[0]!.exit(1);
    expect(children).toHaveLength(2);
    children[1]!.exit(2);
    expect(children).toHaveLength(2);
    expect(sup.state).toBe("stopped");
    expect(exits).toEqual([[2, 1]]);
    expect(sup.write("frame")).toBe(false);
  });

  test("the default limit is three restarts", () => {
    const { children, supervisor: sup } = supervisor();
    sup.start("agent", [], { onLine: () => {} });
    for (let index = 0; index < 3; index++) {
      children.at(-1)!.exit(1);
    }
    expect(children).toHaveLength(4);
    children.at(-1)!.exit(1);
    expect(children).toHaveLength(4);
    expect(sup.state).toBe("stopped");
  });

  test("a session with no exit handler still restarts", () => {
    const { children, supervisor: sup } = supervisor(1);
    sup.start("agent", [], { onLine: () => {} });
    children[0]!.exit(0);
    expect(children).toHaveLength(2);
    children[1]!.exit(0);
    expect(sup.state).toBe("stopped");
  });
});

describe("the launch command", () => {
  test("a restart reuses the command and args it started with", () => {
    const calls: Array<[string, readonly string[]]> = [];
    const children: FakeChild[] = [];
    const sup = new AgentSupervisor({
      spawn: (command, args) => {
        calls.push([command, args]);
        const child = new FakeChild();
        children.push(child);
        return child;
      },
      maxRestarts: 2,
    });
    sup.start("agent", ["--acp"], { onLine: () => {} });
    children[0]!.exit(1);
    children[1]!.exit(1);
    expect(calls).toEqual([
      ["agent", ["--acp"]],
      ["agent", ["--acp"]],
      ["agent", ["--acp"]],
    ]);
  });
});

describe("guard rails", () => {
  test("a child with no stdout still starts", () => {
    const child = new FakeChild();
    Object.defineProperty(child, "stdout", { value: null });
    const sup = new AgentSupervisor({ spawn: () => child });
    sup.start("agent", [], { onLine: () => {} });
    expect(sup.state).toBe("running");
  });

  test("a child with no stdin refuses writes", () => {
    const child = new FakeChild();
    Object.defineProperty(child, "stdin", { value: null });
    const sup = new AgentSupervisor({ spawn: () => child });
    sup.start("agent", [], { onLine: () => {} });
    expect(sup.write("frame")).toBe(false);
  });

  test("stopping before a child exists is harmless", () => {
    const sup = new AgentSupervisor({ spawn: () => new FakeChild() });
    expect(() => sup.stop()).not.toThrow();
    expect(sup.state).toBe("stopped");
  });

  test("an exit with no handlers does not throw", () => {
    const child = new FakeChild();
    const sup = new AgentSupervisor({ spawn: () => child, maxRestarts: 0 });
    sup.start("agent", [], { onLine: () => {} });
    (sup as unknown as { handlers: undefined }).handlers = undefined;
    expect(() => child.exit(0)).not.toThrow();
  });
});

describe("handlers", () => {
  test("restarts reset on a new start", () => {
    const { children, supervisor: sup } = supervisor(3);
    sup.start("agent", [], { onLine: () => {} });
    children[0]!.exit(1);
    expect(sup.restarts).toBe(1);
    sup.start("agent", [], { onLine: () => {} });
    expect(sup.restarts).toBe(0);
  });
});

describe("line routing", () => {
  test("a line reaches the handler registered at start", () => {
    const { children, supervisor: sup } = supervisor();
    const seen: string[] = [];
    sup.start("agent", [], { onLine: (line) => seen.push(line) });
    children[0]!.emit("first");
    expect(seen).toEqual(["first"]);
  });

  test("a child without an exit listener registered still reports through once", () => {
    const { children, supervisor: sup } = supervisor(0);
    const exits: Array<number | null> = [];
    sup.start("agent", [], { onLine: () => {}, onExit: (code) => exits.push(code) });
    children[0]!.exit(7);
    expect(exits).toEqual([7]);
  });
});

describe("the required handlers", () => {
  test("a supervisor always has a line handler before start", () => {
    const sup = new AgentSupervisor({ spawn: () => new FakeChild() });
    expect(sup.state).toBe("idle");
    expect(sup.restarts).toBe(0);
  });
});

describe("exit codes", () => {
  test("the last exit code reaches the exit handler", () => {
    const { children, supervisor: sup } = supervisor(0);
    const codes: Array<number | null> = [];
    sup.start("agent", [], { onLine: () => {}, onExit: (code) => codes.push(code) });
    children[0]!.exit(null);
    expect(codes).toEqual([null]);
  });

  test("a nonzero exit code is reported unchanged", () => {
    const { children, supervisor: sup } = supervisor(0);
    const codes: Array<number | null> = [];
    sup.start("agent", [], { onLine: () => {}, onExit: (code) => codes.push(code) });
    children[0]!.exit(137);
    expect(codes).toEqual([137]);
  });
});

describe("event names", () => {
  test("the supervisor listens for the exit event", () => {
    const child = new FakeChild();
    let listened = "";
    child.once = (event: "exit", listener: (code: number | null) => void) => {
      listened = event;
      child.exits.push(listener);
    };
    const sup = new AgentSupervisor({ spawn: () => child });
    sup.start("agent", [], { onLine: () => {} });
    expect(listened).toBe("exit");
  });
});
