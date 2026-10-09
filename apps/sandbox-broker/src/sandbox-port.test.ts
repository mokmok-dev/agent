import { Effect, Exit } from "effect";
import { afterEach, describe, expect, it } from "vitest";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

import {
  SandboxPort,
  layerReal,
  managerPort,
  type ManagerLike,
  type WrappedArgv,
} from "./sandbox-port.js";

const fullFs = {
  denyRead: [] as Array<string>,
  allowWrite: [] as Array<string>,
  denyWrite: [] as Array<string>,
};

const fullConfig = {
  network: { allowedDomains: [] as string[], deniedDomains: [] as string[] },
  filesystem: fullFs,
};

const makeStub = (
  overrides: {
    initializeError?: unknown;
    resetError?: unknown;
    wrapError?: unknown;
  } = {},
): { stub: ManagerLike; calls: string[] } => {
  const calls: string[] = [];
  const stub: ManagerLike = {
    initialize: async (runtimeConfig, sandboxAskCallback) => {
      calls.push(`initialize:${JSON.stringify(runtimeConfig)}:${typeof sandboxAskCallback}`);
      if (overrides.initializeError !== undefined) {
        throw overrides.initializeError;
      }
    },
    updateConfig: (newConfig) => {
      calls.push(`updateConfig:${JSON.stringify(newConfig)}`);
    },
    reset: async () => {
      calls.push("reset");
      if (overrides.resetError !== undefined) {
        throw overrides.resetError;
      }
    },
    wrapWithSandboxArgv: async (command, binShell, customConfig) => {
      calls.push(`wrap:${command}:${String(binShell)}:${JSON.stringify(customConfig)}`);
      if (overrides.wrapError !== undefined) {
        throw overrides.wrapError;
      }
      return { argv: ["/bin/echo", command], env: { X: "1" } };
    },
    getNetworkRestrictionConfig: () => {
      calls.push("restrictions");
      return { allowedHosts: ["x.example.com"] };
    },
  };
  return { stub, calls };
};

describe("sandbox-port", () => {
  afterEach(async () => {
    await Effect.runPromise(managerPort(SandboxManager).reset()).catch(() => void 0);
  });

  it("maps initialize to the manager with the ask callback", async () => {
    const { stub, calls } = makeStub();
    const port = managerPort(stub);
    const ask = async () => true;
    await Effect.runPromise(port.initialize(fullConfig, ask));
    expect(calls).toEqual([`initialize:${JSON.stringify(fullConfig)}:function`]);
  });

  it("wraps initialize rejection in PortInitializeError with the cause", async () => {
    const { stub } = makeStub({ initializeError: new Error("boom") });
    const port = managerPort(stub);
    const result = await Effect.runPromise(
      Effect.result(port.initialize(fullConfig, async () => true)),
    );
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") {
      const error = result.failure;
      expect(error._tag).toBe("PortInitializeError");
      expect((error.cause as Error).message).toBe("boom");
    }
  });

  it("calls updateConfig synchronously and unmodified", () => {
    const { stub, calls } = makeStub();
    const port = managerPort(stub);
    const config = {
      network: { allowedDomains: ["x"], deniedDomains: [] },
      filesystem: fullFs,
    };
    port.updateConfig(config);
    expect(calls).toEqual([`updateConfig:${JSON.stringify(config)}`]);
  });

  it("treats reset failure as a defect", async () => {
    const { stub } = makeStub({ resetError: new Error("reset boom") });
    const port = managerPort(stub);
    const exit = await Effect.runPromise(Effect.exit(port.reset()));
    expect(Exit.isFailure(exit)).toBe(true);
    if (Exit.isFailure(exit)) {
      expect(String(exit.cause)).toContain("reset boom");
    }
  });

  it("maps wrapArgv with customConfig in the third argument position", async () => {
    const { stub, calls } = makeStub();
    const port = managerPort(stub);
    const cfg = { filesystem: { denyRead: [], allowWrite: ["/tmp"], denyWrite: [] } };
    const wrapped = await Effect.runPromise(port.wrapArgv("echo hi", cfg));
    expect(wrapped).toEqual({ argv: ["/bin/echo", "echo hi"], env: { X: "1" } } as WrappedArgv);
    expect(calls).toEqual([
      'wrap:echo hi:undefined:{"filesystem":{"denyRead":[],"allowWrite":["/tmp"],"denyWrite":[]}}',
    ]);
  });

  it("wraps wrapArgv rejection in PortWrapError with command and cause", async () => {
    const { stub } = makeStub({ wrapError: new Error("wrap boom") });
    const port = managerPort(stub);
    const result = await Effect.runPromise(
      Effect.result(port.wrapArgv("cmd", { filesystem: fullFs })),
    );
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") {
      const error = result.failure;
      expect(error._tag).toBe("PortWrapError");
      expect(error.command).toBe("cmd");
      expect((error.cause as Error).message).toBe("wrap boom");
    }
  });

  it("uses the expected Context tag key", () => {
    expect(SandboxPort.key).toBe("agent/sandbox-broker/SandboxPort");
  });

  it("returns restrictions from the manager", () => {
    const { stub, calls } = makeStub();
    const port = managerPort(stub);
    expect(port.restrictions()).toEqual({ allowedHosts: ["x.example.com"] });
    expect(calls).toEqual(["restrictions"]);
  });

  it("layerReal provides a manager-backed port", async () => {
    const program = Effect.gen(function* () {
      const port = yield* SandboxPort;
      expect(port).toBeDefined();
      expect(typeof port.initialize).toBe("function");
      expect(typeof port.updateConfig).toBe("function");
      expect(typeof port.reset).toBe("function");
      expect(typeof port.wrapArgv).toBe("function");
      expect(typeof port.restrictions).toBe("function");
    }).pipe(Effect.provide(layerReal));
    await Effect.runPromise(program);
  });

  it.skipIf(
    !SandboxManager.isSupportedPlatform() || SandboxManager.checkDependencies().errors.length > 0,
  )("reads back the real manager config after updateConfig", async () => {
    const port = managerPort(SandboxManager);
    const config = {
      network: { allowedDomains: ["real.example.com"], deniedDomains: [] },
      filesystem: fullFs,
    };
    await Effect.runPromise(port.initialize(config, async () => false));
    port.updateConfig(config);
    expect(SandboxManager.getConfig()).toEqual(config);
    expect(SandboxManager.getNetworkRestrictionConfig()).toEqual({
      allowedHosts: ["real.example.com"],
    });
    expect(port.restrictions()).toEqual(SandboxManager.getNetworkRestrictionConfig());
    await Effect.runPromise(port.reset());
  });
});
