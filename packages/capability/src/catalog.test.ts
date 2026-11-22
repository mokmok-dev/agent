import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  AGENTS,
  CATALOG,
  DEFAULT_POLICY,
  PROTOCOLS,
  RUNTIMES,
  route,
  type Agent,
  type Catalog,
  type RoutingPolicy,
} from "./catalog.ts";

const allAvailable: RoutingPolicy = DEFAULT_POLICY;

describe("the shipped catalog", () => {
  test("sandboxes run pi and codex, and the local VM runs the four CLIs", () => {
    const sandbox = CATALOG.runtimes.find((runtime) => runtime.runtime === "cloudflare-sandbox");
    const vm = CATALOG.runtimes.find((runtime) => runtime.runtime === "local-vm");
    expect(sandbox?.agents).toEqual(["pi", "codex"]);
    expect(vm?.agents).toEqual(["pi", "claude-code", "opencode", "grok-build"]);
  });

  test("every agent has one profile, with a protocol, a provider, and a model id", () => {
    for (const agent of AGENTS) {
      const profile = CATALOG.agents[agent];
      expect(profile.agent).toBe(agent);
      expect(PROTOCOLS).toContain(profile.protocol);
      expect(profile.defaultModel.provider).not.toBe("");
      expect(profile.defaultModel.modelId).not.toBe("");
    }
  });

  test("every runtime is listed once", () => {
    expect(CATALOG.runtimes.map((profile) => profile.runtime).sort()).toEqual([...RUNTIMES].sort());
  });

  test("every agent a runtime lists has a profile", () => {
    for (const runtime of CATALOG.runtimes) {
      for (const agent of runtime.agents) expect(CATALOG.agents[agent]).toBeDefined();
    }
  });

  test("pi's protocol is durable and grok build speaks stdio, so the shape is data", () => {
    expect(CATALOG.agents.pi.protocol).toBe("durable");
    expect(CATALOG.agents["grok-build"].protocol).toBe("stdio");
    expect(CATALOG.agents.codex.protocol).toBe("http");
  });

  test("each agent defaults to the provider and model this deployment pays for", () => {
    expect(CATALOG.agents.pi.defaultModel).toEqual({
      provider: "cloudflare-workers-ai",
      modelId: "@cf/deepseek-ai/deepseek-v4.1-flash",
    });
    expect(CATALOG.agents.codex.defaultModel).toEqual({
      provider: "openai",
      modelId: "gpt-5-codex",
    });
    expect(CATALOG.agents["claude-code"].defaultModel).toEqual({
      provider: "anthropic",
      modelId: "claude-opus-4-7",
    });
    expect(CATALOG.agents.opencode.defaultModel).toEqual({
      provider: "anthropic",
      modelId: "claude-sonnet-4-6",
    });
    expect(CATALOG.agents["grok-build"].defaultModel).toEqual({
      provider: "xai",
      modelId: "grok-4.6",
    });
  });

  test("the default policy runs pi in a sandbox first", () => {
    expect(DEFAULT_POLICY.defaultAgent).toBe("pi");
    expect(DEFAULT_POLICY.runtimePreference).toEqual(["cloudflare-sandbox", "local-vm"]);
  });
});

describe("routing an unstated request", () => {
  test("defaults the agent and takes the first available runtime in preference order", () => {
    expect(route({}, CATALOG, allAvailable)).toEqual({
      kind: "routed",
      selection: {
        agent: "pi",
        runtime: "cloudflare-sandbox",
        model: CATALOG.agents.pi.defaultModel,
        protocol: "durable",
      },
    });
  });

  test("skips an unavailable runtime", () => {
    const policy: RoutingPolicy = { ...allAvailable, available: ["local-vm"] };
    expect(route({}, CATALOG, policy)).toMatchObject({
      kind: "routed",
      selection: { runtime: "local-vm" },
    });
  });

  test("takes the agent's own protocol and default model", () => {
    expect(route({ agent: "opencode" }, CATALOG, allAvailable)).toEqual({
      kind: "routed",
      selection: {
        agent: "opencode",
        runtime: "local-vm",
        model: CATALOG.agents.opencode.defaultModel,
        protocol: "stdio",
      },
    });
  });

  test("an explicit model wins over the default", () => {
    const model = { provider: "xai", modelId: "grok-4.7" };
    const result = route({ model }, CATALOG, allAvailable);
    expect(result.kind === "routed" && result.selection.model).toEqual(model);
  });

  test("refuses when no preferred runtime is available", () => {
    const policy: RoutingPolicy = { ...allAvailable, available: [] };
    expect(route({ agent: "pi" }, CATALOG, policy)).toEqual({
      kind: "refused",
      refusal: { kind: "unavailable-runtime", agent: "pi" },
    });
  });

  test("refuses an agent offered by no available runtime, naming the agent", () => {
    const policy: RoutingPolicy = { ...allAvailable, available: ["cloudflare-sandbox"] };
    expect(route({ agent: "claude-code" }, CATALOG, policy)).toEqual({
      kind: "refused",
      refusal: { kind: "unavailable-runtime", agent: "claude-code" },
    });
  });

  test("a runtime absent from the catalog does not match, so the next preference wins", () => {
    const catalog: Catalog = {
      agents: CATALOG.agents,
      runtimes: CATALOG.runtimes.filter((profile) => profile.runtime === "local-vm"),
    };
    expect(route({ agent: "pi" }, catalog, allAvailable)).toMatchObject({
      kind: "routed",
      selection: { runtime: "local-vm" },
    });
  });

  test("a runtime the policy lists but the catalog omits is passed over", () => {
    const catalog: Catalog = { agents: CATALOG.agents, runtimes: [] };
    expect(route({ agent: "pi" }, catalog, allAvailable)).toEqual({
      kind: "refused",
      refusal: { kind: "unavailable-runtime", agent: "pi" },
    });
  });
});

describe("routing an explicit runtime", () => {
  test("uses the named runtime when it supports the agent", () => {
    expect(
      route({ agent: "codex", runtime: "cloudflare-sandbox" }, CATALOG, allAvailable),
    ).toMatchObject({
      kind: "routed",
      selection: { agent: "codex", runtime: "cloudflare-sandbox", protocol: "http" },
    });
  });

  test("honors an explicit runtime even when the policy calls it unavailable", () => {
    const policy: RoutingPolicy = { ...allAvailable, available: [] };
    expect(route({ agent: "pi", runtime: "local-vm" }, CATALOG, policy)).toMatchObject({
      kind: "routed",
      selection: { runtime: "local-vm" },
    });
  });

  test("refuses a runtime that does not run the agent", () => {
    expect(route({ agent: "codex", runtime: "local-vm" }, CATALOG, allAvailable)).toEqual({
      kind: "refused",
      refusal: { kind: "unsupported-combination", agent: "codex", runtime: "local-vm" },
    });
  });

  test("refuses an unknown runtime by name", () => {
    expect(route({ runtime: "mainframe" }, CATALOG, allAvailable)).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-runtime", runtime: "mainframe" },
    });
  });

  test("refuses an unknown agent before it looks at the runtime", () => {
    expect(route({ agent: "nope", runtime: "local-vm" }, CATALOG, allAvailable)).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-agent", agent: "nope" },
    });
  });

  test("reports an unknown agent from the policy too", () => {
    const policy = { ...allAvailable, defaultAgent: "nope" as Agent };
    expect(route({}, CATALOG, policy)).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-agent", agent: "nope" },
    });
  });
});

describe("routing invariants", () => {
  const anyAgent = fc.constantFrom(...AGENTS);
  const anyRuntime = fc.constantFrom(...RUNTIMES);

  test("a routed selection names a runtime that runs its agent", () => {
    fc.assert(
      fc.property(anyAgent, fc.option(anyRuntime, { nil: undefined }), (agent, runtime) => {
        const request = runtime === undefined ? { agent } : { agent, runtime };
        const result = route(request, CATALOG, allAvailable);
        if (result.kind !== "routed") return;
        const chosen = CATALOG.runtimes.find((it) => it.runtime === result.selection.runtime);
        expect(chosen?.agents).toContain(result.selection.agent);
        expect(result.selection.protocol).toBe(CATALOG.agents[agent].protocol);
        expect(result.selection.model).toEqual(CATALOG.agents[agent].defaultModel);
      }),
    );
  });

  test("an explicit agent and runtime are both honored or the request is refused", () => {
    fc.assert(
      fc.property(anyAgent, anyRuntime, (agent, runtime) => {
        const result = route({ agent, runtime }, CATALOG, allAvailable);
        if (result.kind === "routed") {
          expect(result.selection.agent).toBe(agent);
          expect(result.selection.runtime).toBe(runtime);
        } else {
          expect(result.refusal.kind).toBe("unsupported-combination");
        }
      }),
    );
  });
});
