import { describe, expect, test } from "bun:test";
import type { Credential } from "@earendil-works/pi-ai";
import { credentialStoreOver, type ProviderRpc } from "./credentials-rpc.ts";

/**
 * Pi reads a credential with `{ signal }` in its options, and a Durable Object
 * RPC refuses to serialize an `AbortSignal`. These tests pin the adapter that
 * drops those options at the boundary, which is what let a turn reach the model
 * at all.
 */

/** An RPC that records each call, so the test sees what crossed the boundary. */
function rpc(initial: Record<string, Credential> = {}): {
  calls: { method: string; args: unknown[] }[];
  fake: ProviderRpc;
} {
  const held = new Map(Object.entries(initial));
  const calls: { method: string; args: unknown[] }[] = [];
  const fake: ProviderRpc = {
    read: async (providerId: string) => {
      calls.push({ method: "read", args: [providerId] });
      return held.get(providerId);
    },
    list: async () => {
      calls.push({ method: "list", args: [] });
      return [...held.entries()].map(([providerId, credential]) => ({
        providerId,
        type: credential.type,
      }));
    },
    write: async (providerId: string, credential: Credential) => {
      calls.push({ method: "write", args: [providerId, credential] });
      held.set(providerId, credential);
    },
    delete: async (providerId: string) => {
      calls.push({ method: "delete", args: [providerId] });
      held.delete(providerId);
    },
  };
  return { calls, fake };
}

const apiKey: Credential = { type: "api_key", key: "k-1" };

describe("the credential store over RPC", () => {
  test("reads with only the provider id, so no AbortSignal crosses the boundary", async () => {
    const { calls, fake } = rpc({ openai: apiKey });
    const store = credentialStoreOver(fake);
    // Pi's own call shape: the signal is in the options bag.
    const credential = await (
      store.read as (id: string, options: object) => Promise<Credential | undefined>
    )("openai", { signal: new AbortController().signal });
    expect(credential).toEqual(apiKey);
    expect(calls[0]).toEqual({ method: "read", args: ["openai"] });
  });

  test("lists what the object holds", async () => {
    const { fake } = rpc({ openai: apiKey });
    expect(await credentialStoreOver(fake).list()).toEqual([
      { providerId: "openai", type: "api_key" },
    ]);
  });

  test("deletes through the object", async () => {
    const { fake } = rpc({ openai: apiKey });
    const store = credentialStoreOver(fake);
    await store.delete("openai");
    expect(await store.read("openai")).toBeUndefined();
  });
});

describe("modify, which pi runs inside the callback", () => {
  test("writes the value the callback returns", async () => {
    const { calls, fake } = rpc({ openai: apiKey });
    const next = await credentialStoreOver(fake).modify("openai", async (current) => {
      expect(current).toEqual(apiKey);
      return { type: "api_key", key: "k-2" };
    });
    expect(next).toEqual({ type: "api_key", key: "k-2" });
    expect(calls.map((call) => call.method)).toEqual(["read", "write"]);
  });

  test("a callback that returns undefined leaves the entry alone, so a failed refresh is a no-op", async () => {
    const { calls, fake } = rpc({ openai: apiKey });
    const result = await credentialStoreOver(fake).modify("openai", async () => undefined);
    expect(result).toEqual(apiKey);
    expect(calls.map((call) => call.method)).toEqual(["read"]);
  });

  test("a callback that throws propagates and writes nothing", async () => {
    const { calls, fake } = rpc({ openai: apiKey });
    await expect(
      credentialStoreOver(fake).modify("openai", async () => {
        throw new Error("refresh failed");
      }),
    ).rejects.toThrow("refresh failed");
    expect(calls.filter((call) => call.method === "write")).toEqual([]);
  });

  test("a provider with no credential reads as undefined, which is what pi expects", async () => {
    const { fake } = rpc();
    expect(await credentialStoreOver(fake).read("openai")).toBeUndefined();
  });
});
