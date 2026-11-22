import { describe, expect, test } from "bun:test";
import { DEFAULT_POLICY } from "@agent/capability";
import { routingPolicyOf, unknownAgents } from "./routing.ts";

describe("the routing a deployment states", () => {
  test("no configuration keeps the shipped default, so an unconfigured deploy is unchanged", () => {
    expect(routingPolicyOf({})).toEqual(DEFAULT_POLICY);
  });

  test("a stated agent and runtime make the VM the route", () => {
    const policy = routingPolicyOf({
      AGENT_DEFAULT: "claude-code",
      AGENT_RUNTIMES: "local-vm",
      AGENT_AVAILABLE_RUNTIMES: "local-vm",
    });
    expect(policy.defaultAgent).toBe("claude-code");
    expect(policy.runtimePreference).toEqual(["local-vm"]);
    expect(policy.available).toEqual(["local-vm"]);
  });

  test("the runtime list is trimmed and ordered as stated", () => {
    const policy = routingPolicyOf({
      AGENT_RUNTIMES: " local-vm , cloudflare-sandbox ",
      AGENT_AVAILABLE_RUNTIMES: "local-vm",
    });
    expect(policy.runtimePreference).toEqual(["local-vm", "cloudflare-sandbox"]);
  });

  test("a runtime the type does not know is dropped, so a typo cannot pick a route", () => {
    const policy = routingPolicyOf({ AGENT_RUNTIMES: "local-vm,nonsense" });
    expect(policy.runtimePreference).toEqual(["local-vm"]);
  });

  test("a list of only unknown runtimes falls back to the shipped preference", () => {
    expect(routingPolicyOf({ AGENT_RUNTIMES: "nonsense" }).runtimePreference).toEqual(
      DEFAULT_POLICY.runtimePreference,
    );
  });

  test("an empty list falls back rather than leaving no runtime at all", () => {
    expect(routingPolicyOf({ AGENT_RUNTIMES: " , " }).runtimePreference).toEqual(
      DEFAULT_POLICY.runtimePreference,
    );
  });

  test("an unknown agent falls back, and is reported so a caller can refuse the delivery", () => {
    // The policy must name a real agent, so the typo is reported separately
    // rather than becoming a silent route to a different agent.
    const policy = routingPolicyOf({ AGENT_DEFAULT: "nope" });
    expect(policy.defaultAgent).toBe(DEFAULT_POLICY.defaultAgent);
    expect(unknownAgents({ AGENT_DEFAULT: "nope" })).toEqual(["nope"]);
    expect(unknownAgents({ AGENT_DEFAULT: "pi" })).toEqual([]);
    expect(unknownAgents({})).toEqual([]);
  });

  test("whitespace around the agent is trimmed", () => {
    expect(routingPolicyOf({ AGENT_DEFAULT: "  pi  " }).defaultAgent).toBe("pi");
  });

  test("an empty agent string falls back to the shipped default", () => {
    expect(routingPolicyOf({ AGENT_DEFAULT: "   " }).defaultAgent).toBe(
      DEFAULT_POLICY.defaultAgent,
    );
  });
});
