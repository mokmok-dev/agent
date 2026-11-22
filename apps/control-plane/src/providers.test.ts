import { describe, expect, test } from "bun:test";
import { ObjectCredentialStore } from "./credential-store.ts";
import type { StorageHost } from "./storage.ts";
import {
  availableProviders,
  configureProviders,
  PROVIDER_KEYS,
  type ProviderConfig,
} from "./providers.ts";

const config = (overrides: Partial<ProviderConfig> = {}): ProviderConfig => ({
  providers: ["cloudflare-workers-ai"],
  defaultModel: {
    provider: "cloudflare-workers-ai",
    modelId: "@cf/deepseek-ai/deepseek-v4.1-flash",
  },
  ...overrides,
});

const envOf = (values: Record<string, string>) => (name: string) => values[name];

describe("the providers a deployment may register", () => {
  test("registers each named provider, so its models resolve", () => {
    const setup = configureProviders(
      config({ providers: ["cloudflare-workers-ai", "openai"] }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(setup.models.getProvider("openai")).toBeDefined();
    expect(setup.models.getProvider("cloudflare-workers-ai")).toBeDefined();
  });

  test("a provider the deployment did not name has no reachable key", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ ANTHROPIC_API_KEY: "sk-ant-1" }),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("anthropic")).toBeUndefined();
  });

  test("a default model from a provider the deployment did not name is refused", () => {
    expect(
      configureProviders(
        config({
          providers: ["openai"],
          defaultModel: { provider: "anthropic", modelId: "claude-opus-4-7" },
        }),
        envOf({}),
      ),
    ).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-provider", provider: "anthropic" },
    });
  });

  test("keeps the stated default model", () => {
    const model = { provider: "anthropic", modelId: "claude-opus-4-7" };
    const setup = configureProviders(
      config({ providers: ["anthropic"], defaultModel: model }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(setup.defaultModel).toEqual(model);
  });

  test("refuses a provider pi does not know, naming it", () => {
    expect(configureProviders(config({ providers: ["not-a-provider"] }), envOf({}))).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-provider", provider: "not-a-provider" },
    });
  });

  test("refuses an empty default model, because a turn would have nothing to run", () => {
    expect(
      configureProviders(config({ defaultModel: { provider: "", modelId: "" } }), envOf({})),
    ).toEqual({ kind: "refused", refusal: { kind: "no-default-model" } });
  });
});

describe("the keys a provider may read", () => {
  test("a key for an enabled provider reaches pi", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ OPENAI_API_KEY: "sk-1" }),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("openai")).toEqual({
      source: "OPENAI_API_KEY",
      type: "api_key",
    });
  });

  test("a key for a provider this deployment does not run is unreachable", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ OPENAI_API_KEY: "sk-1", ANTHROPIC_API_KEY: "sk-ant-1" }),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("anthropic")).toBeUndefined();
  });

  test("a missing key reads as unauthenticated rather than absent", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("openai")).toBeUndefined();
  });
});

describe("the providers a deployment can actually run", () => {
  test("names only those whose keys are all set", () => {
    expect(
      availableProviders(
        ["openai", "anthropic", "cloudflare-workers-ai"],
        envOf({ OPENAI_API_KEY: "k" }),
      ),
    ).toEqual(["openai"]);
  });

  test("a provider with several keys needs all of them", () => {
    expect(
      availableProviders(["cloudflare-workers-ai"], envOf({ CLOUDFLARE_API_KEY: "k" })),
    ).toEqual([]);
    expect(
      availableProviders(
        ["cloudflare-workers-ai"],
        envOf({ CLOUDFLARE_API_KEY: "k", CLOUDFLARE_ACCOUNT_ID: "a" }),
      ),
    ).toEqual(["cloudflare-workers-ai"]);
  });

  test("an empty value counts as unset, because wrangler stores an empty secret that way", () => {
    expect(availableProviders(["openai"], envOf({ OPENAI_API_KEY: "" }))).toEqual([]);
  });

  test("an unknown provider has no keys and is not advertised as available", () => {
    expect(availableProviders(["nope"], envOf({}))).toEqual(["nope"]);
  });
});

describe("the documented key names", () => {
  test("each entry names at least one key", () => {
    for (const [provider, keys] of Object.entries(PROVIDER_KEYS)) {
      expect(keys.length).toBeGreaterThan(0);
      for (const key of keys) expect(key).toMatch(/^[A-Z][A-Z0-9_]*$/);
      expect(provider).toBe(provider.toLowerCase());
    }
  });

  test("the providers this repository ships a default for are configurable", () => {
    expect(PROVIDER_KEYS["cloudflare-workers-ai"]).toBeDefined();
  });
});

describe("the pieces a caller cannot reach", () => {
  test("the env reader only answers for keys an enabled provider declares", async () => {
    const asked: string[] = [];
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      (name) => {
        asked.push(name);
        return name === "OPENAI_API_KEY" ? "sk" : undefined;
      },
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    await setup.models.checkAuth("openai");
    expect(asked).toEqual(["OPENAI_API_KEY"]);
  });
});

describe("a credential stored on the object", () => {
  const openai = {
    providers: ["openai"],
    defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
  };

  test("authenticates a provider with no environment key, which is how OAuth works", async () => {
    const store = new ObjectCredentialStore(memoryHost());
    await store.put("openai", { type: "api_key", key: "stored-1" });
    const setup = configureProviders(config(openai), envOf({}), store);
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("openai")).toEqual({
      source: "stored credential",
      type: "api_key",
    });
  });

  test("wins over an environment key, so a login takes effect without a redeploy", async () => {
    const store = new ObjectCredentialStore(memoryHost());
    await store.put("openai", { type: "api_key", key: "stored-1" });
    const setup = configureProviders(config(openai), envOf({ OPENAI_API_KEY: "sk-env" }), store);
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    const resolved = await setup.models.getAuth("openai");
    expect(resolved?.source).toBe("stored credential");
    expect(resolved?.auth.apiKey ?? "").toBe("stored-1");
  });

  test("an oauth credential authenticates as oauth, which carries a refresh token", async () => {
    const store = new ObjectCredentialStore(memoryHost());
    await store.put("anthropic", {
      type: "oauth",
      access: "at-1",
      refresh: "rt-1",
      expires: Date.now() + 3_600_000,
    });
    const setup = configureProviders(
      config({
        providers: ["anthropic"],
        defaultModel: { provider: "anthropic", modelId: "claude-opus-4-7" },
      }),
      envOf({}),
      store,
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("anthropic")).toMatchObject({ type: "oauth" });
  });

  test("an empty store leaves the environment as the only source", async () => {
    const store = new ObjectCredentialStore(memoryHost());
    const setup = configureProviders(config(openai), envOf({ OPENAI_API_KEY: "sk-env" }), store);
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("openai")).toEqual({
      source: "OPENAI_API_KEY",
      type: "api_key",
    });
  });
});

/** The in-memory storage a credential store needs. */
function memoryHost(): StorageHost {
  const values = new Map<string, string>();
  return {
    get: async <T>(key: string) => {
      const raw = values.get(key);
      return raw === undefined ? undefined : (JSON.parse(raw) as T);
    },
    put: async <T>(key: string, value: T) => {
      values.set(key, JSON.stringify(value));
    },
    delete: async (key: string) => {
      values.delete(key);
    },
    list: async <T>(options: { prefix: string }) =>
      new Map(
        [...values.entries()]
          .filter(([key]) => key.startsWith(options.prefix))
          .map(([key, raw]) => [key, JSON.parse(raw) as T]),
      ),
  };
}

describe("a deployment with no credential store", () => {
  test("still configures, so a secret-only deployment needs no object", () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ OPENAI_API_KEY: "sk" }),
    );
    expect(setup.kind).toBe("configured");
  });

  test("does not fall back to a stored credential when none is given", async () => {
    const store = new ObjectCredentialStore(memoryHost());
    await store.put("openai", { type: "api_key", key: "stored-1" });
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    // The store exists but was not passed, so it is not consulted.
    expect(await setup.models.checkAuth("openai")).toBeUndefined();
  });
});

describe("the store is passed only when there is one", () => {
  test("a store authenticates while no store leaves the provider unauthenticated", async () => {
    const store = new ObjectCredentialStore(memoryHost());
    await store.put("anthropic", { type: "api_key", key: "stored-1" });
    const base = config({
      providers: ["anthropic"],
      defaultModel: { provider: "anthropic", modelId: "claude-opus-4-7" },
    });
    const withStore = configureProviders(base, envOf({}), store);
    const without = configureProviders(base, envOf({}));
    if (withStore.kind !== "configured" || without.kind !== "configured") {
      throw new Error("expected two configured collections");
    }
    expect(await withStore.models.checkAuth("anthropic")).toMatchObject({ type: "api_key" });
    expect(await without.models.checkAuth("anthropic")).toBeUndefined();
  });
});

describe("a provider this repository supplies itself", () => {
  const ollama = {
    providers: ["ollama-cloud"],
    defaultModel: { provider: "ollama-cloud", modelId: "gemma4:31b" },
  };

  test("is configured when the deployment enables it, and reaches Ollama's endpoint", () => {
    const setup = configureProviders(config(ollama), envOf({}));
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    const provider = setup.models.getProvider("ollama-cloud");
    expect(provider?.id).toBe("ollama-cloud");
    expect(provider?.baseUrl).toBe("https://ollama.com/v1");
  });

  test("starts with no catalog, because the list is fetched from Ollama, not shipped", () => {
    const setup = configureProviders(config(ollama), envOf({}));
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    // A hardcoded list would go stale; the provider fills this on refresh.
    expect(setup.models.getProvider("ollama-cloud")?.getModels()).toEqual([]);
  });

  test("is not registered when the deployment does not enable it", () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    // Pi has no Ollama provider of its own, so an unlisted one is absent
    // rather than reachable through some default.
    expect(setup.models.getProvider("ollama-cloud")).toBeUndefined();
  });

  test("its key is OLLAMA_API_KEY, and a deployment without it reports unauthenticated", async () => {
    const setup = configureProviders(config(ollama), envOf({}));
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("ollama-cloud")).toBeUndefined();
    expect(PROVIDER_KEYS["ollama-cloud"]).toEqual(["OLLAMA_API_KEY"]);
  });

  test("a key in the environment authenticates it", async () => {
    const setup = configureProviders(config(ollama), envOf({ OLLAMA_API_KEY: "k" }));
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("ollama-cloud")).toEqual({
      source: "OLLAMA_API_KEY",
      type: "api_key",
    });
  });
});
