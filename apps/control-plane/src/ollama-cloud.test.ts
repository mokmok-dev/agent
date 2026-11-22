import { describe, expect, test } from "bun:test";
import { InMemoryModelsStore } from "@earendil-works/pi-ai";
import { createModels } from "@earendil-works/pi-ai";
import {
  contextWindowOf,
  modelsFromTags,
  OLLAMA_CLOUD_API_URL,
  OLLAMA_CLOUD_BASE_URL,
  OLLAMA_CLOUD_ID,
  OLLAMA_CLOUD_KEY,
  keyOf,
  ollamaCloudProvider,
} from "./ollama-cloud.ts";

/**
 * Ollama Cloud is not a pi built-in, so these tests pin the pieces pi cannot
 * derive: the endpoint, the key name, and the catalog fetch. The catalog itself
 * comes from Ollama's own API, so the transport is faked and the parse is what
 * is under test.
 */

/** A tag as `GET /api/tags` returns it. */
const tag = (model: string, capabilities: string[] = ["completion", "tools"]) => ({
  name: model,
  model,
  capabilities,
});

/** A transport that answers tags and show from a table, recording each call. */
function transport(tags: unknown[], windows: Record<string, number> = {}) {
  const calls: { url: string; method: string; auth: string; body?: string }[] = [];
  const fetchLike = (async (url: string, init?: RequestInit) => {
    calls.push({
      url: String(url),
      method: init?.method ?? "GET",
      auth: String((init?.headers as Record<string, string> | undefined)?.["authorization"] ?? ""),
      ...(typeof init?.body === "string" ? { body: init.body } : {}),
    });
    if (String(url).endsWith("/tags")) return Response.json({ models: tags });
    const body = JSON.parse(String(init?.body ?? "{}")) as { model?: string };
    const window = windows[body.model ?? ""];
    return Response.json(
      window === undefined ? { model_info: {} } : { model_info: { "x.context_length": window } },
    );
  }) as unknown as typeof fetch;
  return { fetchLike, calls };
}

/**
 * The provider with a fake transport, wired into a real Models collection.
 *
 * `Models.refresh` reads the credential from the store, so the key is stored
 * the way a login would store it; that is also what proves the provider is
 * driven by the same credential path production uses.
 */
async function collection(tags: unknown[], windows: Record<string, number> = {}) {
  const { fetchLike, calls } = transport(tags, windows);
  const credentials = new MemoryCredentials();
  await credentials.put("ollama-cloud", { type: "api_key", key: "k-1" });
  const models = createModels({
    modelsStore: new InMemoryModelsStore(),
    credentials: credentials as never,
  });
  models.setProvider(ollamaCloudProvider({ fetch: fetchLike }));
  return { models, calls, credentials };
}

/** The credential store `Models.refresh` reads, with one key in it. */
class MemoryCredentials {
  readonly #entries = new Map<string, unknown>();

  async put(providerId: string, credential: unknown): Promise<void> {
    this.#entries.set(providerId, credential);
  }

  async read(providerId: string): Promise<unknown> {
    return this.#entries.get(providerId);
  }

  async list(): Promise<readonly unknown[]> {
    return [...this.#entries.keys()].map((providerId) => ({ providerId, type: "api_key" }));
  }

  async modify(
    providerId: string,
    update: (current: unknown) => Promise<unknown>,
  ): Promise<unknown> {
    const next = await update(this.#entries.get(providerId));
    if (next !== undefined) this.#entries.set(providerId, next);
    return next;
  }

  async delete(providerId: string): Promise<void> {
    this.#entries.delete(providerId);
  }
}

describe("the provider", () => {
  test("is named and pointed at Ollama's OpenAI-compatible endpoint", () => {
    const provider = ollamaCloudProvider();
    expect(provider.id).toBe(OLLAMA_CLOUD_ID);
    expect(provider.id).toBe("ollama-cloud");
    expect(provider.baseUrl).toBe(OLLAMA_CLOUD_BASE_URL);
    expect(provider.baseUrl).toBe("https://ollama.com/v1");
    expect(OLLAMA_CLOUD_API_URL).toBe("https://ollama.com/api");
  });

  test("starts with no catalog, because the list is fetched rather than hardcoded", () => {
    expect(ollamaCloudProvider().getModels()).toEqual([]);
  });

  test("authenticates with the key Ollama documents", async () => {
    const resolved = await ollamaCloudProvider().auth.apiKey?.resolve({
      ctx: { env: async (name: string) => (name === "OLLAMA_API_KEY" ? "k-1" : undefined) },
      signal: AbortSignal.timeout(1000),
    } as never);
    expect(OLLAMA_CLOUD_KEY).toBe("OLLAMA_API_KEY");
    expect(resolved).toEqual({ auth: { apiKey: "k-1" }, source: "OLLAMA_API_KEY" });
  });

  test("reports no auth when the key is absent, rather than inventing one", async () => {
    const resolved = await ollamaCloudProvider().auth.apiKey?.resolve({
      ctx: { env: async () => undefined },
      signal: AbortSignal.timeout(1000),
    } as never);
    expect(resolved).toBeUndefined();
  });

  test("a stored credential wins over the environment, so a login takes effect", async () => {
    const resolved = await ollamaCloudProvider().auth.apiKey?.resolve({
      ctx: { env: async () => "from-env" },
      credential: { type: "api_key", key: "from-login" },
      signal: AbortSignal.timeout(1000),
    } as never);
    expect(resolved?.auth.apiKey).toBe("from-login");
    expect(resolved?.source).toBe("stored credential");
  });
});

describe("the catalog fetch", () => {
  test("asks Ollama's tags endpoint with the key as a bearer token", async () => {
    const { models, calls } = await collection([tag("kimi-k2.7-code")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(calls[0]).toEqual({
      url: "https://ollama.com/api/tags",
      method: "GET",
      auth: "Bearer k-1",
    });
  });

  test("registers every chat-capable model with its id", async () => {
    const { models } = await collection([
      tag("kimi-k2.7-code"),
      tag("glm-5.3"),
      tag("gpt-oss:120b"),
    ]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(
      models
        .getProvider(OLLAMA_CLOUD_ID)
        ?.getModels()
        .map((model) => model.id),
    ).toEqual(["kimi-k2.7-code", "glm-5.3", "gpt-oss:120b"]);
  });

  test("a model that cannot complete a chat is left out, so a turn cannot name it", async () => {
    const { models } = await collection([tag("clef:27b", ["decision", "vision"]), tag("glm-5.3")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(
      models
        .getProvider(OLLAMA_CLOUD_ID)
        ?.getModels()
        .map((model) => model.id),
    ).toEqual(["glm-5.3"]);
  });

  test("a thinking model is marked reasoning, so pi can offer a thinking level", async () => {
    const { models } = await collection([tag("glm-5.3", ["completion", "thinking"])]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0]?.reasoning).toBe(true);
  });

  test("a model without thinking is not marked reasoning", async () => {
    const { models } = await collection([tag("plain", ["completion"])]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0]?.reasoning).toBe(false);
  });

  test("each model is reachable on Ollama's endpoint, never a default OpenAI one", async () => {
    const { models } = await collection([tag("glm-5.3")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    const model = models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0];
    expect(model?.baseUrl).toBe(OLLAMA_CLOUD_BASE_URL);
    expect(model?.provider).toBe(OLLAMA_CLOUD_ID);
    expect(model?.api).toBe("openai-completions");
  });

  test("the window Ollama reports for a model is applied", async () => {
    const { models, calls } = await collection([tag("kimi-k2.7-code")], {
      "kimi-k2.7-code": 262_144,
    });
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0]?.contextWindow).toBe(262_144);
    // `POST /api/show` is what carries the window, and it names the model.
    expect(calls[1]?.url).toBe("https://ollama.com/api/show");
    expect(calls[1]?.method).toBe("POST");
    expect(JSON.parse(calls[1]?.body ?? "{}")).toEqual({ model: "kimi-k2.7-code" });
  });

  test("a model whose metadata reports nothing keeps a usable fallback window", async () => {
    const { models } = await collection([tag("glm-5.3")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0]?.contextWindow).toBeGreaterThan(0);
  });

  test("a refresh with no network keeps the stored catalog rather than emptying it", async () => {
    const { models, calls } = await collection([tag("glm-5.3")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    const before = calls.length;
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], allowNetwork: false });
    // Offline, nothing is fetched and the list a turn can name is unchanged.
    expect(calls.length).toBe(before);
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toHaveLength(1);
  });

  test("a refusal keeps the previous catalog rather than emptying it", async () => {
    const credentials = new MemoryCredentials();
    await credentials.put("ollama-cloud", { type: "api_key", key: "k-1" });
    const models = createModels({
      modelsStore: new InMemoryModelsStore(),
      credentials: credentials as never,
    });
    let refusing = false;
    const fetchLike = (async (url: string) => {
      if (refusing) return new Response("nope", { status: 503 });
      if (String(url).endsWith("/tags")) return Response.json({ models: [tag("glm-5.3")] });
      return Response.json({ model_info: {} });
    }) as unknown as typeof fetch;
    models.setProvider(ollamaCloudProvider({ fetch: fetchLike }));
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toHaveLength(1);
    refusing = true;
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toHaveLength(1);
  });

  test("a body that is not a model list is ignored rather than crashing the refresh", async () => {
    const { models } = await collection({ not: "a list" } as never);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toEqual([]);
  });
});

describe("the tags parse", () => {
  test("a tag with neither model nor name is skipped", () => {
    expect(modelsFromTags([{ capabilities: ["completion"] }])).toEqual([]);
  });

  test("an empty id is skipped", () => {
    expect(modelsFromTags([{ model: "", capabilities: ["completion"] }])).toEqual([]);
  });

  test("a missing capability list means no capability, so nothing registers", () => {
    expect(modelsFromTags([{ model: "x" }])).toEqual([]);
  });

  test("a non-string capability is ignored rather than treated as a match", () => {
    expect(modelsFromTags([{ model: "x", capabilities: ["completion", 1] }])).toHaveLength(1);
  });

  test("the model name falls back to the tag name when model is absent", () => {
    expect(modelsFromTags([{ name: "from-name", capabilities: ["completion"] }])[0]?.id).toBe(
      "from-name",
    );
  });

  test("cost is zero, because Ollama bills the account rather than per token", () => {
    const [model] = modelsFromTags([tag("glm-5.3")]);
    expect(model?.cost).toEqual({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 });
  });
});

describe("the context window parse", () => {
  test("reads the namespaced key Ollama reports", () => {
    expect(contextWindowOf({ model_info: { "deepseek_v41.context_length": 1_048_576 } })).toBe(
      1_048_576,
    );
  });

  test("a model_info with no context length reads as unknown", () => {
    expect(contextWindowOf({ model_info: { "x.embedding_length": 0 } })).toBeUndefined();
  });

  test("a non-numeric or non-positive length is unknown, never zero", () => {
    expect(contextWindowOf({ model_info: { "x.context_length": "big" } })).toBeUndefined();
    expect(contextWindowOf({ model_info: { "x.context_length": 0 } })).toBeUndefined();
  });

  test("no model_info at all reads as unknown", () => {
    expect(contextWindowOf({})).toBeUndefined();
  });
});

describe("the exact parse a tag needs", () => {
  test("the registered model declares text input, which is what a chat turn sends", () => {
    expect(modelsFromTags([tag("x")])[0]?.input).toEqual(["text"]);
  });

  test("a non-string capability is dropped rather than counted", () => {
    // The filter keeps only strings, so a numeric entry cannot match a name.
    expect(modelsFromTags([{ model: "x", capabilities: [1, 2] }])).toEqual([]);
  });

  test("a capability list that is not an array reads as no capabilities", () => {
    expect(modelsFromTags([{ model: "x", capabilities: "completion" }])).toEqual([]);
  });

  test("a non-string model field falls back to the tag name", () => {
    expect(
      modelsFromTags([{ model: 42, name: "from-name", capabilities: ["completion"] }]),
    ).toHaveLength(1);
    expect(
      modelsFromTags([{ model: 42, name: "from-name", capabilities: ["completion"] }])[0]?.id,
    ).toBe("from-name");
  });
});

describe("the exact context-length key", () => {
  test("only a key ending in the suffix counts, so a prefix match is not enough", () => {
    expect(contextWindowOf({ model_info: { context_length_hint: 5 } })).toBeUndefined();
  });

  test("a suffix match anywhere in the key is read", () => {
    expect(contextWindowOf({ model_info: { "some.architecture.context_length": 4096 } })).toBe(
      4096,
    );
  });

  test("the first numeric suffix wins when several are present", () => {
    const window = contextWindowOf({
      model_info: { "a.context_length": 8192, "b.context_length": 2048 },
    });
    expect(window).toBe(8192);
  });
});

describe("the names a person sees", () => {
  test("the provider presents itself as Ollama Cloud", () => {
    expect(ollamaCloudProvider().name).toBe("Ollama Cloud");
  });

  test("the key prompt names Ollama Cloud, so the login card is unambiguous", () => {
    expect(ollamaCloudProvider().auth.apiKey?.name).toBe("Ollama Cloud API key");
  });

  test("each fetched model is named after its id, which is what a person selects", async () => {
    const { models } = await collection([tag("glm-5.3")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0]?.name).toBe("glm-5.3");
  });
});

describe("a refresh that cannot use the network", () => {
  test("offline, no request is made and the current list is republished", async () => {
    const { models, calls } = await collection([tag("glm-5.3")]);
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    const before = calls.length;
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], allowNetwork: false });
    expect(calls.length).toBe(before);
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toHaveLength(1);
  });
});

describe("the key a credential carries", () => {
  test("an api key uses its key field", () => {
    expect(keyOf({ type: "api_key", key: "k-1" })).toBe("k-1");
  });

  test("an oauth credential uses its access token, which is where pi stores it", () => {
    expect(keyOf({ type: "oauth", access: "at-1", refresh: "rt-1", expires: 0 } as never)).toBe(
      "at-1",
    );
  });

  test("no credential and a blank token both read as no key", () => {
    expect(keyOf(undefined)).toBeUndefined();
    expect(keyOf({ type: "api_key", key: "" })).toBeUndefined();
    expect(keyOf({ type: "api_key" })).toBeUndefined();
    expect(keyOf({ type: "oauth", access: "" } as never)).toBeUndefined();
  });
});

describe("a response the provider cannot use", () => {
  const credentials = () => {
    const entries = new Map<string, unknown>([["ollama-cloud", { type: "api_key", key: "k-1" }]]);
    return {
      read: async (providerId: string) => entries.get(providerId),
      list: async () => [{ providerId: "ollama-cloud", type: "api_key" }],
      modify: async (_providerId: string, update: (current: unknown) => Promise<unknown>) =>
        update(entries.get("ollama-cloud")),
      delete: async () => {
        entries.delete("ollama-cloud");
      },
    };
  };

  test("a non-array model list is ignored, so a wrong body is not a catalog", async () => {
    const models = createModels({
      modelsStore: new InMemoryModelsStore(),
      credentials: credentials() as never,
    });
    models.setProvider(
      ollamaCloudProvider({ fetch: (async () => Response.json({ models: "nope" })) as never }),
    );
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toEqual([]);
  });

  test("a failed show request leaves the model with its fallback window", async () => {
    const models = createModels({
      modelsStore: new InMemoryModelsStore(),
      credentials: credentials() as never,
    });
    models.setProvider(
      ollamaCloudProvider({
        fetch: (async (url: string) =>
          String(url).endsWith("/tags")
            ? Response.json({ models: [tag("glm-5.3")] })
            : new Response("nope", { status: 500 })) as unknown as typeof fetch,
      }),
    );
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    const model = models.getProvider(OLLAMA_CLOUD_ID)?.getModels()[0];
    expect(model?.contextWindow).toBeGreaterThan(0);
  });

  test("a transport that throws leaves the catalog as it was", async () => {
    const models = createModels({
      modelsStore: new InMemoryModelsStore(),
      credentials: credentials() as never,
    });
    models.setProvider(
      ollamaCloudProvider({
        fetch: (async () => {
          throw new Error("offline");
        }) as never,
      }),
    );
    const result = await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(result.aborted).toBe(false);
    expect(models.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toEqual([]);
  });

  test("the show request is sent as JSON, which is what Ollama reads", async () => {
    const seen: string[] = [];
    const models = createModels({
      modelsStore: new InMemoryModelsStore(),
      credentials: credentials() as never,
    });
    models.setProvider(
      ollamaCloudProvider({
        fetch: (async (url: string, init?: RequestInit) => {
          if (String(url).endsWith("/tags")) return Response.json({ models: [tag("x")] });
          seen.push(String((init?.headers as Record<string, string>)?.["content-type"] ?? ""));
          return Response.json({ model_info: {} });
        }) as unknown as typeof fetch,
      }),
    );
    await models.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(seen).toEqual(["application/json"]);
  });
});

describe("the request the provider sends", () => {
  const models = () => {
    const entries = new Map<string, unknown>([["ollama-cloud", { type: "api_key", key: "k-9" }]]);
    const credentials = {
      read: async (providerId: string) => entries.get(providerId),
      list: async () => [{ providerId: "ollama-cloud", type: "api_key" }],
      modify: async (_id: string, update: (current: unknown) => Promise<unknown>) =>
        update(entries.get("ollama-cloud")),
      delete: async () => undefined,
    };
    return createModels({
      modelsStore: new InMemoryModelsStore(),
      credentials: credentials as never,
    });
  };

  test("the bearer token is the stored key, prefixed as Ollama reads it", async () => {
    const seen: string[] = [];
    const collection = models();
    collection.setProvider(
      ollamaCloudProvider({
        fetch: (async (url: string, init?: RequestInit) => {
          seen.push(String((init?.headers as Record<string, string>)?.["authorization"] ?? ""));
          return String(url).endsWith("/tags")
            ? Response.json({ models: [tag("x")] })
            : Response.json({ model_info: {} });
        }) as unknown as typeof fetch,
      }),
    );
    await collection.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(seen[0]).toBe("Bearer k-9");
  });

  test("a tags body whose models is an array of nothing is an empty catalog, not a refusal", async () => {
    const collection = models();
    collection.setProvider(
      ollamaCloudProvider({ fetch: (async () => Response.json({ models: [] })) as never }),
    );
    await collection.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(collection.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toEqual([]);
  });

  test("a tags body that is not an object at all leaves the catalog empty", async () => {
    const collection = models();
    collection.setProvider(
      ollamaCloudProvider({ fetch: (async () => Response.json(null)) as never }),
    );
    await collection.refresh({ providers: [OLLAMA_CLOUD_ID], force: true });
    expect(collection.getProvider(OLLAMA_CLOUD_ID)?.getModels()).toEqual([]);
  });
});
