import type {
  FilesystemPathEntry,
  NetworkHostPattern,
  SandboxRuntimeConfig,
} from "@anthropic-ai/sandbox-runtime";

import type { AccessRequest, FsTarget, Outcome } from "./access-request.js";

export interface BasePolicy {
  readonly allowedDomains: ReadonlyArray<string>;
  readonly deniedDomains: ReadonlyArray<string>;
  readonly denyRead: ReadonlyArray<FsTarget>;
  readonly allowRead?: ReadonlyArray<FsTarget>;
  readonly allowWrite: ReadonlyArray<FsTarget>;
  readonly denyWrite: ReadonlyArray<FsTarget>;
}

export interface Policy {
  readonly base: BasePolicy;
  readonly approved: ReadonlyArray<AccessRequest>;
}

export const initialPolicy = (base: BasePolicy): Policy => ({ base, approved: [] });

export type TakeEffect =
  | { readonly _tag: "Immediate" }
  | { readonly _tag: "NextWrap" }
  | { readonly _tag: "Never" };

const effectByTag: { readonly [K in AccessRequest["_tag"]]: TakeEffect } = {
  AllowHost: { _tag: "Immediate" },
  AllowRead: { _tag: "NextWrap" },
  AllowWrite: { _tag: "NextWrap" },
};

export const takeEffect = (request: AccessRequest, outcome: Outcome): TakeEffect => {
  if (outcome._tag === "Denied") {
    return { _tag: "Never" };
  }
  return effectByTag[request._tag];
};

const targetKey = (target: FsTarget): string =>
  typeof target === "string" ? `pattern:${target}` : `literal:${target.path}`;

const requestKey = (request: AccessRequest): string => {
  switch (request._tag) {
    case "AllowHost":
      return `host:${request.pattern}`;
    case "AllowRead":
      return `read:${targetKey(request.target)}`;
    case "AllowWrite":
      return `write:${targetKey(request.target)}`;
  }
};

export const approveInto = (policy: Policy, request: AccessRequest): Policy => {
  const key = requestKey(request);
  const exists = policy.approved.some((r) => requestKey(r) === key);
  if (exists) {
    return policy;
  }
  return { base: policy.base, approved: [...policy.approved, request] };
};

const uniqueBy = <T>(key: (value: T) => string, values: ReadonlyArray<T>): T[] => {
  const seen = new Set<string>();
  return values.filter((value) => {
    const k = key(value);
    if (seen.has(k)) {
      return false;
    }
    seen.add(k);
    return true;
  });
};

const mapToEntries = (targets: ReadonlyArray<FsTarget>): FilesystemPathEntry[] =>
  targets.map(toPathEntry);

export const sessionConfigFor = (policy: Policy): SandboxRuntimeConfig => {
  const allowedDomains = uniqueBy(
    (domain: string) => domain,
    [
      ...policy.base.allowedDomains,
      ...policy.approved
        .filter((r): r is Extract<typeof r, { _tag: "AllowHost" }> => r._tag === "AllowHost")
        .map((r) => r.pattern),
    ],
  );
  const filesystem: SandboxRuntimeConfig["filesystem"] = {
    denyRead: mapToEntries(policy.base.denyRead),
    allowWrite: mapToEntries(policy.base.allowWrite),
    denyWrite: mapToEntries(policy.base.denyWrite),
  };
  if (policy.base.allowRead !== undefined) {
    filesystem.allowRead = mapToEntries(policy.base.allowRead);
  }
  return {
    network: {
      allowedDomains,
      deniedDomains: [...policy.base.deniedDomains],
    },
    filesystem,
  };
};

export const wrapConfigFor = (
  policy: Policy,
): { readonly filesystem: SandboxRuntimeConfig["filesystem"] } => {
  const allowRead = uniqueBy(targetKey, [
    ...(policy.base.allowRead ?? []),
    ...policy.approved
      .filter((r): r is Extract<typeof r, { _tag: "AllowRead" }> => r._tag === "AllowRead")
      .map((r) => r.target),
  ]);
  const allowWrite = uniqueBy(targetKey, [
    ...policy.base.allowWrite,
    ...policy.approved
      .filter((r): r is Extract<typeof r, { _tag: "AllowWrite" }> => r._tag === "AllowWrite")
      .map((r) => r.target),
  ]);
  return {
    filesystem: {
      denyRead: mapToEntries(policy.base.denyRead),
      allowRead: mapToEntries(allowRead),
      allowWrite: mapToEntries(allowWrite),
      denyWrite: mapToEntries(policy.base.denyWrite),
    },
  };
};

export const patternCovers = (pattern: string, host: string, port: number | undefined): boolean => {
  const lastColon = pattern.lastIndexOf(":");
  const patternPort =
    lastColon > 0 && /^\d+$/.test(pattern.slice(lastColon + 1))
      ? Number(pattern.slice(lastColon + 1))
      : undefined;
  const patternHost = patternPort === undefined ? pattern : pattern.slice(0, lastColon);
  if (patternHost !== host) {
    return false;
  }
  if (patternPort === undefined) {
    return true;
  }
  return port === patternPort;
};

export type ResolvedGrant = {
  readonly request: AccessRequest;
  readonly outcome: Outcome;
};

/**
 * Everything a session accumulates. `open` goes false when the session's scope
 * releases, which is what stops a grant the caller approves afterwards from
 * reaching a sandbox that has already been torn down.
 */
export interface SessionState {
  readonly policy: Policy;
  readonly resolved: ReadonlyArray<ResolvedGrant>;
  readonly open: boolean;
}

export const closeSession = (state: SessionState): SessionState => ({ ...state, open: false });

export const ledgerAnswerFor = (
  resolved: ReadonlyArray<ResolvedGrant>,
  ask: NetworkHostPattern,
): boolean | undefined => {
  const matches = resolved.filter(
    (g): g is typeof g & { request: { _tag: "AllowHost" } } =>
      g.request._tag === "AllowHost" && patternCovers(g.request.pattern, ask.host, ask.port),
  );
  const last = matches.at(-1);
  if (last === undefined) {
    return undefined;
  }
  return last.outcome._tag === "Approved";
};

export const patternOfAsk = (ask: NetworkHostPattern): string =>
  ask.port === undefined ? ask.host : `${ask.host}:${ask.port}`;

export const toPathEntry = (target: FsTarget): FilesystemPathEntry =>
  typeof target === "string" ? target : { path: target.path, literal: true };
