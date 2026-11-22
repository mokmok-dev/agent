import {
  AGENTS,
  DEFAULT_POLICY,
  RUNTIMES,
  type Agent,
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
}

const isAgent = (value: string): value is Agent => (AGENTS as readonly string[]).includes(value);

const isRuntime = (value: string): value is Runtime =>
  (RUNTIMES as readonly string[]).includes(value);

/** A comma separated list, trimmed, with the entries the type does not know dropped. */
function runtimeList(stated: string): Runtime[] {
  const names = stated
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name !== "");
  return names.filter(isRuntime) as Runtime[];
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
  const statedAgent = env.AGENT_DEFAULT?.trim();
  const preference = runtimeList(env.AGENT_RUNTIMES ?? "");
  const available = runtimeList(env.AGENT_AVAILABLE_RUNTIMES ?? "");
  return {
    defaultAgent:
      statedAgent !== undefined && statedAgent !== "" && isAgent(statedAgent)
        ? statedAgent
        : DEFAULT_POLICY.defaultAgent,
    runtimePreference: preference.length > 0 ? preference : DEFAULT_POLICY.runtimePreference,
    available: available.length > 0 ? available : DEFAULT_POLICY.available,
  };
}

/** The agent names a deployment stated that the catalog does not know. */
export function unknownAgents(env: RoutingEnv): string[] {
  const stated = env.AGENT_DEFAULT?.trim();
  if (stated === undefined || stated === "") return [];
  return isAgent(stated) ? [] : [stated];
}
