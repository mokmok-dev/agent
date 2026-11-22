import {
  AGENTS,
  CATALOG,
  DEFAULT_POLICY,
  RUNTIMES,
  type Agent,
  type Catalog,
  type ModelRef,
  type RoutingPolicy,
  type Runtime,
} from "@agent/capability";

/**
 * The routing a deployment states, as configuration.
 *
 * A delivery's route decides where its turn runs. The shipped default prefers a
 * sandbox, so a deployment that wants the local VM states it here rather than
 * editing code. The agent and its runtime must agree, which the router checks.
 */

/** The env names a deployment sets. All are optional, so the default stands. */
export interface RoutingEnv {
  /** The agent a delivery runs when its session states none. */
  AGENT_DEFAULT?: string;
  /** Runtimes in preference order, comma separated. */
  AGENT_RUNTIMES?: string;
  /** Runtimes that are reachable right now, comma separated. */
  AGENT_AVAILABLE_RUNTIMES?: string;
  /**
   * The model the deployment's agent runs, as `provider:modelId`. The router
   * pins what this names, so a deployment that registers one provider cannot be
   * routed to a model of another, which pi then reports as `no_model`.
   */
  PI_DEFAULT_MODEL?: string;
}

const isAgent = (value: string): value is Agent => (AGENTS as readonly string[]).includes(value);

const isRuntime = (value: string): value is Runtime =>
  (RUNTIMES as readonly string[]).includes(value);

/**
 * A comma separated list, trimmed, with unknown names dropped. `isRuntime`
 * already rejects a blank entry, because the empty string is not a runtime.
 */
function runtimeList(stated: string): Runtime[] {
  return stated
    .split(",")
    .map((name) => name.trim())
    .filter(isRuntime);
}

/**
 * The policy a deployment states, or the shipped default for each part it does
 * not state.
 *
 * An agent name the catalog does not know falls back to the shipped default,
 * because a policy's default agent has to be a real one for the router to work.
 * A deployment that names an unknown agent is a configuration mistake with no
 * safe route, and `unknownAgents` reports it so a caller can refuse the whole
 * delivery rather than quietly running a different agent.
 */
export function routingPolicyOf(env: RoutingEnv): RoutingPolicy {
  const statedAgent = (env.AGENT_DEFAULT ?? "").trim();
  const preference = runtimeList(env.AGENT_RUNTIMES ?? "");
  const available = runtimeList(env.AGENT_AVAILABLE_RUNTIMES ?? "");
  return {
    // `isAgent("")` is false, so a blank agent needs no separate check.
    defaultAgent: isAgent(statedAgent) ? statedAgent : DEFAULT_POLICY.defaultAgent,
    runtimePreference: preference.length > 0 ? preference : DEFAULT_POLICY.runtimePreference,
    available: available.length > 0 ? available : DEFAULT_POLICY.available,
  };
}

/**
 * The agent names a deployment stated that the catalog does not know. A blank
 * field is unstated rather than a typo, so it is not reported.
 */
export function unknownAgents(env: RoutingEnv): string[] {
  const stated = (env.AGENT_DEFAULT ?? "").trim();
  return stated === "" || isAgent(stated) ? [] : [stated];
}

/**
 * The catalog a deployment routes against: the shipped one, with the
 * deployment's model pinned onto the agent it names.
 *
 * The router answers with a model, and pi resolves that model against the
 * providers the deployment registered. A catalog whose default names a provider
 * the deployment did not register produces a turn that fails `no_model`, so
 * `PI_PROVIDERS` and `PI_DEFAULT_MODEL` have to be stated together.
 */
export function catalogOf(env: RoutingEnv, agent: Agent): Catalog {
  const model = modelRefOf(env.PI_DEFAULT_MODEL);
  const profile = CATALOG.agents[agent];
  if (model === undefined || profile === undefined) return CATALOG;
  return {
    ...CATALOG,
    agents: { ...CATALOG.agents, [agent]: { ...profile, defaultModel: model } },
  };
}

/** `provider:modelId`, or `undefined` when the deployment states none. */
function modelRefOf(stated: string | undefined): ModelRef | undefined {
  const text = (stated ?? "").trim();
  const separator = text.indexOf(":");
  if (separator < 0) return undefined;
  const provider = text.slice(0, separator);
  const modelId = text.slice(separator + 1);
  return provider === "" || modelId === "" ? undefined : { provider, modelId };
}
