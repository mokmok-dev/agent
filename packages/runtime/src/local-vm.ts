import { spawnProcess } from "./process/proc.ts";
import type { RuntimeTarget } from "./port.ts";

/**
 * The local VM target: the agent is a child of the gateway process, which
 * dials out to the control plane. This is what runs Claude Code, OpenCode,
 * Grok Build, and pi on a machine the operator owns.
 */
export function localVmTarget(): RuntimeTarget {
  return { runtime: "local-vm", spawn: spawnProcess };
}
