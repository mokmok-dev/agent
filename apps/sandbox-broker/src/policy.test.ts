import { describe, expect, it } from "vitest";
import * as fc from "fast-check";

import type { FsTarget, Outcome } from "./access-request.js";
import {
  type BasePolicy,
  type SessionState,
  approveInto,
  closeSession,
  initialPolicy,
  ledgerAnswerFor,
  patternCovers,
  patternOfAsk,
  sessionConfigFor,
  takeEffect,
  toPathEntry,
  wrapConfigFor,
} from "./policy.js";

const hostArb = fc.string({ minLength: 1 }).filter((s) => !s.includes(":"));
const digitPortArb = fc.integer({ min: 1, max: 65535 });

const parsePattern = (pattern: string): { host: string; port: number | undefined } => {
  const lastColon = pattern.lastIndexOf(":");
  if (lastColon > 0 && /^\d+$/.test(pattern.slice(lastColon + 1))) {
    return {
      host: pattern.slice(0, lastColon),
      port: Number(pattern.slice(lastColon + 1)),
    };
  }
  return { host: pattern, port: undefined };
};

const referenceCovers = (pattern: string, host: string, port: number | undefined): boolean => {
  const parsed = parsePattern(pattern);
  if (parsed.host !== host) return false;
  if (parsed.port === undefined) return true;
  return port === parsed.port;
};

describe("policy", () => {
  it("reports Immediate for AllowHost, NextWrap for fs, Never for denials", () => {
    const approved: Outcome = { _tag: "Approved" };
    const denied: Outcome = { _tag: "Denied" };
    expect(takeEffect({ _tag: "AllowHost", pattern: "x" }, approved)).toEqual({
      _tag: "Immediate",
    });
    expect(takeEffect({ _tag: "AllowRead", target: "/x" }, approved)).toEqual({ _tag: "NextWrap" });
    expect(takeEffect({ _tag: "AllowWrite", target: "/x" }, approved)).toEqual({
      _tag: "NextWrap",
    });
    expect(takeEffect({ _tag: "AllowHost", pattern: "x" }, denied)).toEqual({ _tag: "Never" });
    expect(takeEffect({ _tag: "AllowRead", target: "/x" }, denied)).toEqual({ _tag: "Never" });
    expect(takeEffect({ _tag: "AllowWrite", target: "/x" }, denied)).toEqual({ _tag: "Never" });
  });

  it("deduplicates approved requests by tag and target, keeping oldest first", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const a = { _tag: "AllowHost", pattern: "a" } as const;
    const b = { _tag: "AllowHost", pattern: "b" } as const;
    const p1 = approveInto(policy, a);
    const p2 = approveInto(p1, a);
    const p3 = approveInto(p2, b);
    expect(p3.approved).toEqual([a, b]);
  });

  it("treats a bare string target and a Literal with the same path as different entries", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const stringTarget = { _tag: "AllowRead", target: "/x" } as const;
    const literalTarget = { _tag: "AllowRead", target: { _tag: "Literal", path: "/x" } } as const;
    const next = approveInto(approveInto(policy, stringTarget), literalTarget);
    expect(next.approved).toEqual([stringTarget, literalTarget]);
  });

  it("builds the session config with base filesystem and merged network allowlist", () => {
    const policy = initialPolicy({
      allowedDomains: ["base.example.com"],
      deniedDomains: ["blocked.example.com"],
      denyRead: [".git/config"],
      allowRead: ["/tmp/read"],
      allowWrite: ["/tmp/write"],
      denyWrite: ["/tmp/deny"],
    });
    const withHost = approveInto(policy, { _tag: "AllowHost", pattern: "api.github.com" });
    const duplicated = approveInto(withHost, { _tag: "AllowHost", pattern: "api.github.com" });
    const config = sessionConfigFor(duplicated);
    expect(config.network).toEqual({
      allowedDomains: ["base.example.com", "api.github.com"],
      deniedDomains: ["blocked.example.com"],
    });
    expect(config.filesystem.denyRead).toEqual([".git/config"]);
    expect(config.filesystem.allowRead).toEqual(["/tmp/read"]);
    expect(config.filesystem.allowWrite).toEqual(["/tmp/write"]);
    expect(config.filesystem.denyWrite).toEqual(["/tmp/deny"]);
  });

  it("omits allowRead from session config when the base omits it", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const config = sessionConfigFor(policy);
    expect(Object.hasOwn(config.filesystem, "allowRead")).toBe(false);
  });

  it("always carries all four filesystem lists in wrap config and never network", () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: ["/denied-read"],
      allowRead: ["/allowed-read"],
      allowWrite: ["/base-write"],
      denyWrite: ["/denied-write"],
    };
    const policy = approveInto(initialPolicy(base), { _tag: "AllowWrite", target: "/extra-write" });
    const cfg = wrapConfigFor(policy);
    expect("network" in cfg).toBe(false);
    expect(cfg.filesystem.denyRead).toEqual(["/denied-read"]);
    expect(cfg.filesystem.allowRead).toEqual(["/allowed-read"]);
    expect(cfg.filesystem.allowWrite).toEqual(["/base-write", "/extra-write"]);
    expect(cfg.filesystem.denyWrite).toEqual(["/denied-write"]);
  });

  it("matches a port-less pattern on any port and a port pattern only on that port", () => {
    expect(patternCovers("api.example.com", "api.example.com", undefined)).toBe(true);
    expect(patternCovers("api.example.com", "api.example.com", 443)).toBe(true);
    expect(patternCovers("api.example.com:443", "api.example.com", 443)).toBe(true);
    expect(patternCovers("api.example.com:443", "api.example.com", 80)).toBe(false);
    expect(patternCovers("api.example.com:443", "api.example.com", undefined)).toBe(false);
    expect(patternCovers("api.example.com", "other.example.com", undefined)).toBe(false);
    expect(patternCovers("a:b", "a:b", undefined)).toBe(true);
    expect(patternCovers("a:b", "a", undefined)).toBe(false);
  });

  it("agrees with a reference implementation for arbitrary patterns and asks", () => {
    fc.assert(
      fc.property(
        fc.string({ minLength: 1 }),
        hostArb,
        fc.option(digitPortArb, { nil: undefined }),
        (pattern, host, port) => {
          expect(patternCovers(pattern, host, port)).toBe(referenceCovers(pattern, host, port));
        },
      ),
    );
  });

  it("lets the most recent decided host pattern override an earlier one", () => {
    const ask = { host: "api.example.com", port: undefined };
    const a = { _tag: "AllowHost", pattern: "api.example.com" } as const;
    const resolved = [
      { request: a, outcome: { _tag: "Denied" } as Outcome },
      { request: a, outcome: { _tag: "Approved" } as Outcome },
    ];
    expect(ledgerAnswerFor(resolved, ask)).toBe(true);
    expect(ledgerAnswerFor([...resolved].reverse(), ask)).toBe(false);
  });

  it("returns undefined when no resolved pattern covers the ask", () => {
    const ask = { host: "unknown.example.com", port: undefined };
    expect(ledgerAnswerFor([], ask)).toBeUndefined();
  });

  it("builds an AllowHost pattern from an ask with or without port", () => {
    expect(patternOfAsk({ host: "x.example.com", port: undefined })).toBe("x.example.com");
    expect(patternOfAsk({ host: "x.example.com", port: 443 })).toBe("x.example.com:443");
  });

  it("converts both FsTarget forms to srt FilesystemPathEntry", () => {
    expect(toPathEntry("/tmp/foo")).toBe("/tmp/foo");
    expect(toPathEntry({ _tag: "Literal", path: "/tmp/foo" } as FsTarget)).toEqual({
      path: "/tmp/foo",
      literal: true,
    });
  });

  it("preserves submission order on the approved list", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const r1 = { _tag: "AllowHost", pattern: "first" } as const;
    const r2 = { _tag: "AllowHost", pattern: "second" } as const;
    const r3 = { _tag: "AllowRead", target: "/third" } as const;
    const next = approveInto(approveInto(approveInto(policy, r1), r2), r3);
    expect(next.approved).toEqual([r1, r2, r3]);
  });

  it("deduplicates base allowedDomains in the session config", () => {
    const policy = initialPolicy({
      allowedDomains: ["a.example.com", "a.example.com", "b.example.com"],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const config = sessionConfigFor(policy);
    expect(config.network.allowedDomains).toEqual(["a.example.com", "b.example.com"]);
  });

  it("keeps AllowRead and AllowWrite with the same target as separate entries", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const read = { _tag: "AllowRead", target: "/x" } as const;
    const write = { _tag: "AllowWrite", target: "/x" } as const;
    const next = approveInto(approveInto(policy, read), write);
    expect(next.approved).toEqual([read, write]);
  });

  it("keeps two Literal filesystem targets with different paths as separate entries", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const a = {
      _tag: "AllowRead",
      target: { _tag: "Literal", path: "/a" },
    } as const;
    const b = {
      _tag: "AllowRead",
      target: { _tag: "Literal", path: "/b" },
    } as const;
    const next = approveInto(approveInto(policy, a), b);
    expect(next.approved).toEqual([a, b]);
  });

  it("keeps AllowWrite requests with different targets as separate entries", () => {
    const policy = initialPolicy({
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    });
    const a = { _tag: "AllowWrite", target: "/a" } as const;
    const b = { _tag: "AllowWrite", target: "/b" } as const;
    const next = approveInto(approveInto(policy, a), b);
    expect(next.approved).toEqual([a, b]);
  });

  it("includes approved AllowRead targets in the wrap config", () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const policy = approveInto(
      approveInto(initialPolicy(base), { _tag: "AllowRead", target: "/s" }),
      { _tag: "AllowRead", target: { _tag: "Literal", path: "/l" } },
    );
    const cfg = wrapConfigFor(policy);
    expect(cfg.filesystem.allowRead).toEqual(["/s", { path: "/l", literal: true }]);
  });

  it("does not put approved host grants into the wrap filesystem lists", () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowRead: ["/base-read"],
      allowWrite: ["/base-write"],
      denyWrite: [],
    };
    const policy = approveInto(initialPolicy(base), {
      _tag: "AllowHost",
      pattern: "api.example.com",
    });
    const cfg = wrapConfigFor(policy);
    expect(cfg.filesystem.allowRead).toEqual(["/base-read"]);
    expect(cfg.filesystem.allowWrite).toEqual(["/base-write"]);
  });

  it("only puts approved host grants into the network allowlist", () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const policy = approveInto(
      approveInto(approveInto(initialPolicy(base), { _tag: "AllowHost", pattern: "h" }), {
        _tag: "AllowRead",
        target: "/r",
      }),
      { _tag: "AllowWrite", target: "/w" },
    );
    const config = sessionConfigFor(policy);
    expect(config.network.allowedDomains).toEqual(["h"]);
  });

  it("ignores filesystem grants when resolving a network ask", () => {
    const ask = { host: "api.example.com", port: undefined };
    const resolved = [
      {
        request: { _tag: "AllowHost", pattern: "api.example.com" } as const,
        outcome: { _tag: "Denied" } as Outcome,
      },
      {
        request: { _tag: "AllowRead", target: "/x" } as const,
        outcome: { _tag: "Approved" } as Outcome,
      },
      {
        request: { _tag: "AllowHost", pattern: "api.example.com" } as const,
        outcome: { _tag: "Approved" } as Outcome,
      },
    ];
    expect(ledgerAnswerFor(resolved, ask)).toBe(true);
    expect(ledgerAnswerFor([...resolved].reverse(), ask)).toBe(false);
  });

  it("ignores a trailing filesystem grant when picking the last matching host", () => {
    const ask = { host: "api.example.com", port: undefined };
    const resolved = [
      {
        request: { _tag: "AllowHost", pattern: "api.example.com" } as const,
        outcome: { _tag: "Denied" } as Outcome,
      },
      {
        request: { _tag: "AllowRead", target: "/x" } as const,
        outcome: { _tag: "Approved" } as Outcome,
      },
    ];
    expect(ledgerAnswerFor(resolved, ask)).toBe(false);
  });

  it("uses the last matching host grant when there are at least three matches", () => {
    const ask = { host: "api.example.com", port: undefined };
    const resolved = [
      {
        request: { _tag: "AllowHost", pattern: "api.example.com" } as const,
        outcome: { _tag: "Denied" } as Outcome,
      },
      {
        request: { _tag: "AllowHost", pattern: "api.example.com" } as const,
        outcome: { _tag: "Approved" } as Outcome,
      },
      {
        request: { _tag: "AllowHost", pattern: "api.example.com" } as const,
        outcome: { _tag: "Denied" } as Outcome,
      },
    ];
    expect(ledgerAnswerFor(resolved, ask)).toBe(false);
  });

  it("distinguishes a digit-only host from a port suffix", () => {
    expect(patternCovers("443", "443", undefined)).toBe(true);
  });

  it("treats a leading colon as part of the host, not a port separator", () => {
    expect(patternCovers(":443", ":443", undefined)).toBe(true);
  });

  it("rejects a port suffix that starts with digits but has trailing characters", () => {
    expect(patternCovers("host:80extra", "host:80extra", undefined)).toBe(true);
  });

  it("rejects a port suffix that ends with digits but has leading characters", () => {
    expect(patternCovers("host:abc80", "host:abc80", undefined)).toBe(true);
  });

  it("closes a session keeping its policy and its ledger, and only flips open", () => {
    const base: BasePolicy = {
      allowedDomains: [],
      deniedDomains: [],
      denyRead: [],
      allowWrite: [],
      denyWrite: [],
    };
    const request = { _tag: "AllowHost", pattern: "api.example.com" } as const;
    const state: SessionState = {
      policy: approveInto(initialPolicy(base), request),
      resolved: [{ request, outcome: { _tag: "Approved" } }],
      open: true,
    };

    expect(closeSession(state)).toEqual({
      policy: state.policy,
      resolved: state.resolved,
      open: false,
    });
  });
});
