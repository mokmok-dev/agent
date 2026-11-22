import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { openDurableObjectSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/cloudflare";
import { createRegistry } from "@earendil-works/pi-durable";
import type { MutableModels } from "@earendil-works/pi-ai";
import type { ModelRef } from "@agent/capability";
import { ConversationBase } from "@agent/conversation";
import type { Env } from "./worker.ts";
import { configureProviders, type ProviderConfig, type ProviderRefused } from "./providers.ts";
import { piTurnRunner } from "./pi-turn-runner.ts";
import type { TurnRunner } from "./turn-runner.ts";

/**
 * The pi runner a Durable Object supplies.
 *
 * Pi stores its conversation in the same SQLite the session lives in, so the
 * turn's history survives an eviction with the session record. The model
 * collection resolves its credential from the Worker's own environment, which
 * is how pi's Cloudflare provider expects to reach a key.
 *
 * This is the one integration that needs a real deployment to prove: workerd
 * does not start on the development host, and `cloudflare:workers` is not
 * importable outside it.
 */
export function piRunnerFor(state: DurableObjectState, env: Env): TurnRunner {
  return piRunnerOver(() => open(state, env));
}

/**
 * The runner over a conversation factory. Opening is memoized, so a second turn
 * of the same session reuses the conversation it already opened: the transcript
 * is one history, and reopening would lose the in-memory half of it.
 */
export function piRunnerOver(openOnce: () => Promise<ConversationBase>): TurnRunner {
  let opened: Promise<ConversationBase> | undefined;
  return piTurnRunner({
    conversation: () => {
      opened ??= openOnce();
      return opened;
    },
  });
}

export async function open(state: DurableObjectState, env: Env): Promise<ConversationBase> {
  const storage = await openDurableObjectSqliteStorage(state.storage);
  const setup = setupFor(env);
  const conversation = await ConversationBase.open(storage, {
    models: setup.models,
    registry: createRegistry(),
    context: BACKGROUND_CONTEXT,
  });
  // Pin the deployment's default so a turn that states no model still runs one
  // the operator chose, rather than a provider's own default.
  await conversation.setModel(setup.defaultModel);
  return conversation;
}

/**
 * The configured collection, or the error a misconfiguration deserves. Split
 * from `open` so everything except the Durable Object's SQLite is testable.
 */
export function setupFor(env: Env): { models: MutableModels; defaultModel: ModelRef } {
  const setup = configureProviders(providerConfigOf(env), (name) => {
    const value = env[name as keyof Env];
    return typeof value === "string" ? value : undefined;
  });
  if (setup.kind === "refused") throw new Error(refusalText(setup.refusal));
  return { models: setup.models, defaultModel: setup.defaultModel };
}

/**
 * The model collection. `PI_PROVIDERS` names the providers and
 * `PI_DEFAULT_MODEL` names the model as `provider:modelId`, so a deployment
 * picks its providers by configuration rather than by code.
 */
export function modelsFor(env: Env): MutableModels {
  return setupFor(env).models;
}

/** The provider list and default model a deployment states. */
export function providerConfigOf(env: Env): ProviderConfig {
  const providers = (env.PI_PROVIDERS ?? "")
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name !== "");
  return {
    providers: providers.length > 0 ? providers : ["cloudflare-workers-ai"],
    defaultModel: modelRefOf(env.PI_DEFAULT_MODEL),
  };
}

/**
 * `provider:modelId`, with Workers AI as the fallback this repository ships. A
 * value with no separator yields an empty model id, and a leading separator
 * yields an empty provider, which `configureProviders` refuses by name rather
 * than this function guessing.
 */
function modelRefOf(stated: string | undefined): ModelRef {
  const text = (stated ?? "").trim();
  if (text === "") {
    return { provider: "cloudflare-workers-ai", modelId: "@cf/deepseek-ai/deepseek-v4.1-flash" };
  }
  const separator = text.indexOf(":");
  if (separator < 0) return { provider: text, modelId: "" };
  return { provider: text.slice(0, separator), modelId: text.slice(separator + 1) };
}

/** A refusal as the sentence a person reads in the failed turn. */
export function refusalText(refusal: ProviderRefused): string {
  switch (refusal.kind) {
    case "unknown-provider":
      return `PI_PROVIDERS names an unknown provider: ${refusal.provider}`;
    case "no-default-model":
      return "PI_DEFAULT_MODEL must be provider:modelId";
    case "missing-key":
      return `${refusal.provider} needs ${refusal.keys.join(" and ")}`;
  }
}
