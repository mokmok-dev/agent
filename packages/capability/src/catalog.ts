/**
 * The three choices a session makes about how it runs. The rest of the system
 * branches on these names, never on a vendor product or a process shape.
 */

export const AGENTS = ["pi", "codex", "claude-code", "opencode", "grok-build"] as const;

export type Agent = (typeof AGENTS)[number];

export const RUNTIMES = ["cloudflare-sandbox", "local-vm"] as const;

export type Runtime = (typeof RUNTIMES)[number];

/**
 * How the control plane speaks to an agent. `durable` means the agent loop is
 * a library in the host process; `stdio` means a child process speaks a line
 * protocol; `http` means a server the host reaches over the network.
 */
export const PROTOCOLS = ["durable", "stdio", "http"] as const;

export type Protocol = (typeof PROTOCOLS)[number];

/** A model is named by its provider and id, as pi-ai resolves it. */
export type ModelRef = { provider: string; modelId: string };

export type AgentProfile = {
  agent: Agent;
  protocol: Protocol;
  /** The default model when the session names none. */
  defaultModel: ModelRef;
};

/**
 * What one runtime can run. An agent absent from this list cannot be selected
 * on this runtime at all.
 */
export type RuntimeProfile = {
  runtime: Runtime;
  agents: readonly Agent[];
};

/**
 * The agent registry is the single owner of each agent's protocol and default
 * model, and each runtime only lists the agents it runs. A runtime therefore
 * cannot carry a second, divergent copy of an agent's settings.
 */
export type Catalog = {
  agents: Readonly<Record<Agent, AgentProfile>>;
  runtimes: readonly RuntimeProfile[];
};

/** What a session asks for. Every field is optional, so a caller states only its intent. */
export type RoutingRequest = {
  agent?: string;
  runtime?: string;
  model?: ModelRef;
};

/** The deployment's standing choices, used where a session states none. */
export type RoutingPolicy = {
  /** The agent a session runs when it names none. */
  defaultAgent: Agent;
  /** Runtimes in preference order. The first available one that supports the agent wins. */
  runtimePreference: readonly Runtime[];
  /** Runtimes that are actually reachable right now. */
  available: readonly Runtime[];
};

export type Selection = {
  agent: Agent;
  runtime: Runtime;
  model: ModelRef;
  protocol: Protocol;
};

export type Refusal =
  | { kind: "unknown-agent"; agent: string }
  | { kind: "unknown-runtime"; runtime: string }
  | { kind: "unsupported-combination"; agent: Agent; runtime: Runtime }
  | { kind: "unavailable-runtime"; agent: Agent };

export type RoutingResult =
  { kind: "routed"; selection: Selection } | { kind: "refused"; refusal: Refusal };

/**
 * Decide the agent, runtime, model, and protocol for one session. An explicit
 * choice is honored or refused; only an unstated one falls back to the policy.
 */
export function route(
  request: RoutingRequest,
  catalog: Catalog,
  policy: RoutingPolicy,
): RoutingResult {
  const agentName = request.agent ?? policy.defaultAgent;
  const profile = catalog.agents[agentName as Agent];
  if (profile === undefined) return refuse({ kind: "unknown-agent", agent: agentName });

  if (request.runtime !== undefined) {
    const named = catalog.runtimes.find((it) => it.runtime === request.runtime);
    if (named === undefined) return refuse({ kind: "unknown-runtime", runtime: request.runtime });
    if (!named.agents.includes(profile.agent)) {
      return refuse({
        kind: "unsupported-combination",
        agent: profile.agent,
        runtime: named.runtime,
      });
    }
    return selected(profile, named.runtime, request.model);
  }

  for (const name of policy.runtimePreference) {
    if (!policy.available.includes(name)) continue;
    const candidate = catalog.runtimes.find((it) => it.runtime === name);
    if (candidate !== undefined && candidate.agents.includes(profile.agent)) {
      return selected(profile, candidate.runtime, request.model);
    }
  }
  return refuse({ kind: "unavailable-runtime", agent: profile.agent });
}

function selected(
  profile: AgentProfile,
  runtime: Runtime,
  model: ModelRef | undefined,
): RoutingResult {
  return {
    kind: "routed",
    selection: {
      agent: profile.agent,
      runtime,
      model: model ?? profile.defaultModel,
      protocol: profile.protocol,
    },
  };
}

function refuse(refusal: Refusal): RoutingResult {
  return { kind: "refused", refusal };
}

/**
 * The catalog this deployment ships. Cloudflare sandboxes run pi and codex;
 * the local VM runs pi, Claude Code, OpenCode, and Grok Build.
 */
export const CATALOG: Catalog = {
  agents: {
    pi: {
      agent: "pi",
      protocol: "durable",
      defaultModel: {
        provider: "cloudflare-workers-ai",
        modelId: "@cf/deepseek-ai/deepseek-v4.1-flash",
      },
    },
    codex: {
      agent: "codex",
      protocol: "http",
      defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
    },
    "claude-code": {
      agent: "claude-code",
      protocol: "stdio",
      defaultModel: { provider: "anthropic", modelId: "claude-opus-4-7" },
    },
    opencode: {
      agent: "opencode",
      protocol: "stdio",
      defaultModel: { provider: "anthropic", modelId: "claude-sonnet-4-6" },
    },
    "grok-build": {
      agent: "grok-build",
      protocol: "stdio",
      defaultModel: { provider: "xai", modelId: "grok-4.6" },
    },
  },
  runtimes: [
    { runtime: "cloudflare-sandbox", agents: ["pi", "codex"] },
    { runtime: "local-vm", agents: ["pi", "claude-code", "opencode", "grok-build"] },
  ],
};

export const DEFAULT_POLICY: RoutingPolicy = {
  defaultAgent: "pi",
  runtimePreference: ["cloudflare-sandbox", "local-vm"],
  available: ["cloudflare-sandbox", "local-vm"],
};
