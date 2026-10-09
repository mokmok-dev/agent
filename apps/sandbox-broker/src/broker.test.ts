import { Effect, Fiber, Layer } from "effect";
import type { NetworkHostPattern } from "@anthropic-ai/sandbox-runtime";
import { describe, expect, it } from "vitest";

import type { AccessRequest, RequestId } from "./access-request.js";
import { ApprovalDesk, ApprovalDeskLayer } from "./approval-desk.js";
import type { BasePolicy } from "./policy.js";
import {
  PortInitializeError,
  PortWrapError,
  SandboxPort,
  type SandboxPort as ISandboxPort,
  type WrappedArgv,
} from "./sandbox-port.js";
import { type SandboxBroker, withSession } from "./broker.js";

interface FakePort {
  readonly port: ISandboxPort;
  readonly calls: string[];
  readonly configs: Array<import("@anthropic-ai/sandbox-runtime").SandboxRuntimeConfig>;
  readonly askCalls: NetworkHostPattern[];
  readonly shells: Array<string | undefined>;
  readonly ask:
    | ((params: { host: string; port: number | undefined }) => Promise<boolean>)
    | undefined;
  readonly restrictions: { allowedHosts: string[]; deniedHosts?: string[] };
}

const makeFakePort = (
  options: { initializeError?: unknown; wrapError?: unknown } = {},
): FakePort => {
  const calls: string[] = [];
  const configs: Array<import("@anthropic-ai/sandbox-runtime").SandboxRuntimeConfig> = [];
  const askCalls: NetworkHostPattern[] = [];
  const shells: Array<string | undefined> = [];
  let restrictions: { allowedHosts: string[]; deniedHosts?: string[] } = { allowedHosts: [] };
  let capturedAsk:
    | ((params: { host: string; port: number | undefined }) => Promise<boolean>)
    | undefined;
  const port: ISandboxPort = {
    initialize: (config, ask) =>
      Effect.gen(function* () {
        calls.push("initialize");
        configs.push(config);
        capturedAsk = (params) => {
          askCalls.push(params);
          return ask(params);
        };
        if (options.initializeError !== undefined) {
          return yield* Effect.fail(new PortInitializeError({ cause: options.initializeError }));
        }
      }),
    updateConfig: (config) => {
      calls.push("updateConfig");
      configs.push(config);
      restrictions = { allowedHosts: config.network.allowedDomains };
      if (config.network.deniedDomains.length > 0) {
        restrictions.deniedHosts = config.network.deniedDomains;
      }
    },
    reset: () =>
      Effect.sync(() => {
        calls.push("reset");
      }),
    wrapArgv: (command, customConfig, binShell) =>
      Effect.gen(function* () {
        calls.push(`wrap:${command}:${JSON.stringify(customConfig)}`);
        shells.push(binShell);
        if (options.wrapError !== undefined) {
          return yield* Effect.fail(new PortWrapError({ command, cause: options.wrapError }));
        }
        return { argv: ["/bin/echo", command], env: {} } as WrappedArgv;
      }),
    restrictions: () => restrictions,
  };
  return {
    port,
    calls,
    configs,
    askCalls,
    shells,
    get ask() {
      return capturedAsk;
    },
    get restrictions() {
      return restrictions;
    },
  };
};

const emptyBase: BasePolicy = {
  allowedDomains: [],
  deniedDomains: [],
  denyRead: [],
  allowWrite: [],
  denyWrite: [],
};

const awaitPending = (desk: ApprovalDesk, attempts = 200) =>
  Effect.gen(function* () {
    let n = 0;
    while (true) {
      const pending = yield* desk.pending();
      if (pending.length > 0) {
        return pending;
      }
      n += 1;
      if (n > attempts) {
        return yield* Effect.die("no pending decision arrived in time");
      }
      yield* Effect.sleep("5 millis");
    }
  });

const runWithFake = <A, E>(
  base: BasePolicy,
  use: (session: SandboxBroker, port: FakePort) => Effect.Effect<A, E, ApprovalDesk>,
): Promise<{ result: A; port: FakePort }> => {
  const fake = makeFakePort();
  const layer = Layer.succeed(SandboxPort, fake.port);
  const program = withSession(base, (session) => use(session, fake)).pipe(
    Effect.provide(Layer.merge(layer, ApprovalDeskLayer)),
  );
  return Effect.runPromise(program).then((result) => ({ result, port: fake }));
};

describe("broker", () => {
  it("initializes and resets around the session use", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const { port } = await runWithFake(base, (session) =>
      Effect.gen(function* () {
        yield* session.wrap("echo hi");
      }),
    );
    expect(port.calls).toEqual([
      "initialize",
      'wrap:echo hi:{"filesystem":{"denyRead":[],"allowRead":[],"allowWrite":[],"denyWrite":[]}}',
      "reset",
    ]);
  });

  it("records an attempted ask when the proxy asks and resolves true on approval", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const request: AccessRequest = { _tag: "AllowHost", pattern: "x.example.com:443" };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        const fiber = yield* Effect.forkDetach(
          Effect.promise(async () => {
            expect(port.ask).toBeDefined();
            return port.ask!({ host: "x.example.com", port: 443 });
          }),
          { startImmediately: true },
        );
        yield* Effect.sleep("10 millis");
        const pending = yield* desk.pending();
        expect(pending).toEqual([{ requestId: "1", request, origin: "Attempted" }]);
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        const answer = yield* Fiber.join(fiber);
        expect(answer).toBe(true);
        expect(port.calls).toContain("updateConfig");
        expect(port.restrictions.allowedHosts).toContain("x.example.com:443");
      }),
    );
  });

  it("resolves false when an attempted ask is denied and does not update config", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        const fiber = yield* Effect.forkDetach(
          Effect.promise(async () => port.ask!({ host: "y.example.com", port: undefined })),
          { startImmediately: true },
        );
        yield* Effect.sleep("10 millis");
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Denied" },
        });
        const answer = yield* Fiber.join(fiber);
        expect(answer).toBe(false);
        expect(port.calls).not.toContain("updateConfig");
      }),
    );
  });

  it("remembers a denied host so a retry loop does not ask again", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        const f1 = yield* Effect.forkDetach(
          Effect.promise(async () => port.ask!({ host: "z.example.com", port: undefined })),
          { startImmediately: true },
        );
        yield* Effect.sleep("10 millis");
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Denied" },
        });
        yield* Fiber.join(f1);
        expect(port.askCalls).toHaveLength(1);
        const answer2 = yield* Effect.promise(async () =>
          Promise.race([
            port.ask!({ host: "z.example.com", port: undefined }),
            new Promise<never>((_, reject) =>
              setTimeout(() => reject(new Error("second ask did not use cached answer")), 100),
            ),
          ]),
        );
        expect(answer2).toBe(false);
        expect(port.askCalls).toHaveLength(2);
        expect(yield* desk.pending()).toHaveLength(0);
      }),
    );
  });

  it("materializes an approved predicted host grant into the live config", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const request: AccessRequest = { _tag: "AllowHost", pattern: "api.github.com" };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        const fiber = yield* Effect.forkDetach(session.submit(request), { startImmediately: true });
        yield* Effect.sleep("5 millis");
        const pending = yield* desk.pending();
        expect(pending).toEqual([{ requestId: "1", request, origin: "Predicted" }]);
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        const granted = yield* Fiber.join(fiber);
        expect(granted.effect).toEqual({ _tag: "Immediate" });
        expect(port.restrictions.allowedHosts).toEqual(["api.github.com"]);
      }),
    );
  });

  it("reports filesystem grants as NextWrap and folds them into the next wrap", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: ["/base"],
      denyWrite: [],
    };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        yield* session.wrap("first");
        const fiber = yield* Effect.forkDetach(
          session.submit({ _tag: "AllowWrite", target: "/extra" }),
          { startImmediately: true },
        );
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        const granted = yield* Fiber.join(fiber);
        expect(granted.effect).toEqual({ _tag: "NextWrap" });
        expect(port.calls.filter((c) => c === "updateConfig")).toHaveLength(0);
        yield* session.wrap("second");
        const wrapCalls = port.calls.filter((c) => c.startsWith("wrap:"));
        expect(wrapCalls[0]).toContain('"allowWrite":["/base"]');
        expect(wrapCalls[1]).toContain('"allowWrite":["/base","/extra"]');
      }),
    );
  });

  it("does not leak a reset after initialize fails", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const fake = makeFakePort({ initializeError: new Error("init boom") });
    const layer = Layer.succeed(SandboxPort, fake.port);
    const result = await Effect.runPromise(
      Effect.result(
        withSession(base, () => Effect.void).pipe(
          Effect.provide(Layer.merge(layer, ApprovalDeskLayer)),
        ),
      ),
    );
    expect(result._tag).toBe("Failure");
    expect(fake.calls).toEqual(["initialize"]);
  });

  it("does not materialize a grant the caller approves after the session closed", async () => {
    const fake = makeFakePort();
    const program = Effect.gen(function* () {
      const desk = yield* ApprovalDesk;
      yield* withSession(emptyBase, (session) =>
        Effect.gen(function* () {
          yield* Effect.forkDetach(
            session.submit({ _tag: "AllowHost", pattern: "late.example.com" }),
            { startImmediately: true },
          );
          yield* awaitPending(desk);
        }),
      );

      const [entry] = yield* awaitPending(desk);
      if (entry === undefined) {
        return yield* Effect.die("the parked request disappeared");
      }
      yield* desk.decide({
        _tag: "Decision",
        requestId: entry.requestId,
        outcome: { _tag: "Approved" },
      });
      yield* Effect.sleep("50 millis");
    }).pipe(Effect.provide(Layer.merge(Layer.succeed(SandboxPort, fake.port), ApprovalDeskLayer)));

    await Effect.runPromise(program);
    expect(fake.calls).toEqual(["initialize", "reset"]);
  });

  it("hands the port the shell the caller named", async () => {
    const fake = makeFakePort();
    const program = withSession(
      emptyBase,
      (session) => Effect.asVoid(session.wrap("echo hi")),
      "/bin/sh",
    ).pipe(Effect.provide(Layer.merge(Layer.succeed(SandboxPort, fake.port), ApprovalDeskLayer)));

    await Effect.runPromise(program);
    expect(fake.shells).toEqual(["/bin/sh"]);
  });

  it("resolves false when the ask callback encounters an internal failure", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const fake = makeFakePort();
    const failingDeskLayer = Layer.succeed(ApprovalDesk, {
      submit: () => Effect.fail(new Error("desk boom")),
      pending: () => Effect.succeed([]),
      decide: () => Effect.fail(new Error("desk boom")),
    } as unknown as ApprovalDesk);
    const program = withSession(base, (_session) =>
      Effect.gen(function* () {
        const answer = yield* Effect.promise(async () =>
          fake.ask!({ host: "x.example.com", port: undefined }),
        );
        expect(answer).toBe(false);
      }),
    ).pipe(Effect.provide(Layer.merge(Layer.succeed(SandboxPort, fake.port), failingDeskLayer)));
    await Effect.runPromise(program);
  });

  it("remembers an approved host so a retry does not ask again", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        const f1 = yield* Effect.forkDetach(
          Effect.promise(async () => port.ask!({ host: "a.example.com", port: undefined })),
          { startImmediately: true },
        );
        yield* Effect.sleep("10 millis");
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        yield* Fiber.join(f1);
        expect(port.askCalls).toHaveLength(1);
        const answer2 = yield* Effect.promise(async () =>
          Promise.race([
            port.ask!({ host: "a.example.com", port: undefined }),
            new Promise<never>((_, reject) =>
              setTimeout(() => reject(new Error("second ask did not use cached answer")), 100),
            ),
          ]),
        );
        expect(answer2).toBe(true);
        expect(port.askCalls).toHaveLength(2);
        expect(yield* desk.pending()).toHaveLength(0);
      }),
    );
  });

  it("concurrently approved hosts all land in the recorded config", async () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    await runWithFake(base, (session, port) =>
      Effect.gen(function* () {
        const desk = yield* ApprovalDesk;
        const f1 = yield* Effect.forkDetach(session.submit({ _tag: "AllowHost", pattern: "a" }), {
          startImmediately: true,
        });
        const f2 = yield* Effect.forkDetach(session.submit({ _tag: "AllowHost", pattern: "b" }), {
          startImmediately: true,
        });
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        yield* desk.decide({
          _tag: "Decision",
          requestId: "2" as RequestId,
          outcome: { _tag: "Approved" },
        });
        yield* Fiber.join(f1);
        yield* Fiber.join(f2);
        expect(port.restrictions.allowedHosts).toContain("a");
        expect(port.restrictions.allowedHosts).toContain("b");
      }),
    );
  });
});
