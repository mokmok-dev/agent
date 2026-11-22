import { createProvider, envApiKeyAuth, type Model, type Provider } from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions.lazy";

/**
 * Ollama Cloud as a pi-ai provider.
 *
 * Ollama Cloud answers the OpenAI chat-completions subset at
 * `https://ollama.com/v1`, authenticated with a bearer API key, so it plugs in
 * through pi's existing completions API. Pi has no built-in Ollama provider, so
 * this module supplies the base URL, the key name, and the catalog.
 *
 * The catalog is fetched, not hardcoded: `GET https://ollama.com/api/tags` is
 * the account's live list, and pi persists it through a `ModelsStore` with the
 * remote validators, so a restart reuses the cached list and a changed list
 * costs one conditional request.
 */

export const OLLAMA_CLOUD_ID = "ollama-cloud";
export const OLLAMA_CLOUD_BASE_URL = "https://ollama.com/v1";
/** The native API, which is where the model list lives. */
export const OLLAMA_CLOUD_API_URL = "https://ollama.com/api";
/** The key Ollama Cloud reads. It is a bearer token, not a query parameter. */
export const OLLAMA_CLOUD_KEY = "OLLAMA_API_KEY";

/** The output cap a model gets when the catalog reports no smaller one. */
const DEFAULT_MAX_TOKENS = 8_192;
/** The window to assume when a model's metadata does not report one. */
const DEFAULT_CONTEXT_WINDOW = 131_072;

/**
 * One entry of `GET /api/tags`. Only the fields this provider reads are typed;
 * Ollama sends more, and a missing field is tolerated rather than fatal.
 */
type Tag = {
  model?: unknown;
  name?: unknown;
  capabilities?: unknown;
};

/** The context length a model reports, from `POST /api/show`. */
type Show = {
  model_info?: Record<string, unknown>;
  details?: Record<string, unknown>;
};

/**
 * The models a key can reach, as pi model definitions.
 *
 * A model is kept only if it can complete a chat: `capabilities` carries
 * `completion` for those, and a decision-only model such as `clef` would fail a
 * turn rather than answer it.
 */
export function modelsFromTags(tags: readonly Tag[]): Model<"openai-completions">[] {
  const models: Model<"openai-completions">[] = [];
  for (const tag of tags) {
    const id = idOf(tag);
    if (id === undefined) continue;
    if (!capabilitiesOf(tag).includes("completion")) continue;
    models.push({
      id,
      name: id,
      api: "openai-completions",
      provider: OLLAMA_CLOUD_ID,
      baseUrl: OLLAMA_CLOUD_BASE_URL,
      reasoning: capabilitiesOf(tag).includes("thinking"),
      input: ["text"],
      // The real window comes from `POST /api/show`; this is the fallback when
      // the tags response is all a caller has.
      contextWindow: DEFAULT_CONTEXT_WINDOW,
      maxTokens: DEFAULT_MAX_TOKENS,
      // Ollama bills the account rather than per token, so no rate is reported.
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    });
  }
  return models;
}

function idOf(tag: Tag): string | undefined {
  const id = typeof tag.model === "string" ? tag.model : tag.name;
  if (typeof id !== "string" || id === "") return undefined;
  return id;
}

function capabilitiesOf(tag: Tag): readonly string[] {
  return Array.isArray(tag.capabilities)
    ? tag.capabilities.filter((value): value is string => typeof value === "string")
    : [];
}

/**
 * The context length `POST /api/show` reports. The key is namespaced by the
 * model's architecture, so the suffix is what is matched rather than a fixed
 * name.
 */
export function contextWindowOf(show: Show): number | undefined {
  for (const [key, value] of Object.entries(show.model_info ?? {})) {
    if (!key.endsWith(".context_length")) continue;
    if (typeof value === "number" && value > 0) return value;
  }
  return undefined;
}

/**
 * The provider, as pi registers it.
 *
 * `fetchModels` runs pi's refresh phase: it asks Ollama for the list, applies
 * each model's reported window, and publishes through the store. A refresh that
 * cannot fetch publishes the current list, so an offline start or a failing
 * endpoint leaves the cached catalog in place rather than emptying it.
 */
export function ollamaCloudProvider(
  options: {
    /** The HTTP transport, so a test answers without the network. */
    fetch?: typeof fetch;
  } = {},
): Provider<"openai-completions"> {
  const transport = options.fetch ?? fetch;
  // What the last successful refresh produced, kept so a refresh that cannot
  // fetch republishes it instead of an empty list.
  let current: Model<"openai-completions">[] = [];
  return createProvider<"openai-completions">({
    id: OLLAMA_CLOUD_ID,
    name: "Ollama Cloud",
    baseUrl: OLLAMA_CLOUD_BASE_URL,
    // `envApiKeyAuth` builds the api-key half; `ProviderAuth` wraps it, because
    // a provider may offer both an api key and an OAuth flow.
    auth: { apiKey: envApiKeyAuth("Ollama Cloud API key", [OLLAMA_CLOUD_KEY]) },
    // A dynamic catalog: the list is whatever the last refresh stored.
    models: [],
    fetchModels: async (context) => {
      // An offline refresh is a no-op *as a fetch*: it republishes what is
      // already known, which is what keeps a cached catalog alive across an
      // eviction that cannot reach the network.
      if (!context.allowNetwork) return current;
      const key = keyOf(context.credential);
      // No key is the same situation as no network: pi cannot ask Ollama, so
      // the current list is what a turn may still name.
      if (key === undefined) return current;
      const tags = await listModels(transport, key, context.signal);
      // A refusal is not an empty catalog: returning the current list means a
      // failing endpoint costs nothing.
      if (tags === undefined) return current;
      const models = modelsFromTags(tags);
      // Each model's window is a second request; a failure keeps the fallback
      // rather than dropping a model that can otherwise answer.
      await applyContextWindows(transport, key, models, context.signal);
      current = models;
      return models;
    },
    api: openAICompletionsApi(),
  });
}

/**
 * The bearer token a credential carries, or `undefined` when there is none.
 * An OAuth credential holds its token in `access`, an api key in `key`.
 */
export function keyOf(
  credential: { type: string; key?: string; access?: string } | undefined,
): string | undefined {
  if (credential === undefined) return undefined;
  const key = credential.type === "oauth" ? credential.access : credential.key;
  return key === undefined || key === "" ? undefined : key;
}

/** `GET /api/tags`, or `undefined` when Ollama refuses or is unreachable. */
async function listModels(
  transport: typeof fetch,
  key: string,
  signal: AbortSignal,
): Promise<readonly Tag[] | undefined> {
  try {
    const response = await transport(`${OLLAMA_CLOUD_API_URL}/tags`, {
      headers: { accept: "application/json", authorization: `Bearer ${key}` },
      signal,
    });
    if (!response.ok) return undefined;
    const body = (await response.json()) as { models?: unknown };
    if (!Array.isArray(body.models)) return undefined;
    return body.models as readonly Tag[];
  } catch {
    // A refresh is best effort: pi keeps the previous list on a throw, and a
    // refusal is the same outcome without the noise.
    return undefined;
  }
}

/** Fill each model's window from `POST /api/show`, in place. */
async function applyContextWindows(
  transport: typeof fetch,
  key: string,
  models: Model<"openai-completions">[],
  signal: AbortSignal,
): Promise<void> {
  for (const model of models) {
    const window = await windowFor(transport, key, model.id, signal);
    if (window !== undefined) model.contextWindow = window;
  }
}

async function windowFor(
  transport: typeof fetch,
  key: string,
  id: string,
  signal: AbortSignal,
): Promise<number | undefined> {
  try {
    const response = await transport(`${OLLAMA_CLOUD_API_URL}/show`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${key}`,
      },
      body: JSON.stringify({ model: id }),
      signal,
    });
    if (!response.ok) return undefined;
    return contextWindowOf((await response.json()) as Show);
  } catch {
    return undefined;
  }
}
