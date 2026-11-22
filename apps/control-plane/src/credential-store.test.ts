import { describe, expect, test } from "bun:test";
import type { Credential } from "@earendil-works/pi-ai";
import { ObjectCredentialStore } from "./credential-store.ts";
import { ProviderCredentialObject } from "./providers-do.ts";
import type { StorageHost } from "./storage.ts";

/** The in-memory host a Durable Object's storage presents. */
class Host implements StorageHost {
  readonly values = new Map<string, string>();

  async get<T>(key: string): Promise<T | undefined> {
    const raw = this.values.get(key);
    return raw === undefined ? undefined : (JSON.parse(raw) as T);
  }

  async put<T>(key: string, value: T): Promise<void> {
    this.values.set(key, JSON.stringify(value));
  }

  async delete(key: string): Promise<void> {
    this.values.delete(key);
  }

  async list<T>(options: { prefix: string }): Promise<Map<string, T>> {
    return new Map(
      [...this.values.entries()]
        .filter(([key]) => key.startsWith(options.prefix))
        .map(([key, raw]) => [key, JSON.parse(raw) as T]),
    );
  }
}

const apiKey: Credential = { type: "api_key", key: "sk-1" };
const oauth: Credential = {
  type: "oauth",
  access: "at-1",
  refresh: "rt-1",
  expires: 1_700_000_000_000,
};

const store = () => {
  const host = new Host();
  return { host, credentials: new ObjectCredentialStore(host) };
};

describe("storing a credential", () => {
  test("an api key round-trips", async () => {
    const { credentials } = store();
    await credentials.put("openai", apiKey);
    expect(await credentials.read("openai")).toEqual(apiKey);
  });

  test("an oauth credential round-trips with its refresh token and expiry", async () => {
    const { credentials } = store();
    await credentials.put("anthropic", oauth);
    expect(await credentials.read("anthropic")).toEqual(oauth);
  });

  test("a provider with no credential reads as undefined, not a failure", async () => {
    const { credentials } = store();
    expect(await credentials.read("openai")).toBeUndefined();
  });

  test("a second write replaces the first, because one provider has one credential", async () => {
    const { credentials } = store();
    await credentials.put("openai", apiKey);
    await credentials.put("openai", { type: "api_key", key: "sk-2" });
    expect(await credentials.read("openai")).toEqual({ type: "api_key", key: "sk-2" });
  });

  test("the credential is under a key named for the provider, and nowhere else", async () => {
    const { host, credentials } = store();
    await credentials.put("openai", apiKey);
    expect([...host.values.keys()]).toEqual(["credential:openai"]);
  });
});

describe("listing what is stored", () => {
  test("names each provider and its type, never the secret", async () => {
    const { credentials } = store();
    await credentials.put("openai", apiKey);
    await credentials.put("anthropic", oauth);
    const listed = await credentials.list();
    expect([...listed].sort((a, b) => a.providerId.localeCompare(b.providerId))).toEqual([
      { providerId: "anthropic", type: "oauth" },
      { providerId: "openai", type: "api_key" },
    ]);
    expect(JSON.stringify(listed)).not.toContain("sk-1");
    expect(JSON.stringify(listed)).not.toContain("at-1");
  });

  test("an empty store lists nothing", async () => {
    const { credentials } = store();
    expect(await credentials.list()).toEqual([]);
  });
});

describe("modifying a credential", () => {
  test("the writer sees the current credential, which a refresh needs", async () => {
    const { credentials } = store();
    await credentials.put("anthropic", oauth);
    await credentials.modify("anthropic", async (current) => {
      expect(current).toEqual(oauth);
      return { type: "oauth", access: "at-2", refresh: "rt-1", expires: 1_700_000_100_000 };
    });
    expect(await credentials.read("anthropic")).toMatchObject({ access: "at-2" });
  });

  test("returning undefined leaves the entry as it was, so a failed refresh is a no-op", async () => {
    const { credentials } = store();
    await credentials.put("anthropic", oauth);
    const result = await credentials.modify("anthropic", async () => undefined);
    expect(result).toEqual(oauth);
    expect(await credentials.read("anthropic")).toEqual(oauth);
  });

  test("a writer that throws propagates, and the stored credential is untouched", async () => {
    const { credentials } = store();
    await credentials.put("anthropic", oauth);
    await expect(
      credentials.modify("anthropic", async () => {
        throw new Error("refresh failed");
      }),
    ).rejects.toThrow("refresh failed");
    expect(await credentials.read("anthropic")).toEqual(oauth);
  });

  test("two writes to one provider leave one credential, not a merge", async () => {
    const { credentials } = store();
    await Promise.all([
      credentials.put("openai", { type: "api_key", key: "a" }),
      credentials.put("openai", { type: "api_key", key: "b" }),
    ]);
    const stored = await credentials.read("openai");
    expect(stored?.type).toBe("api_key");
    const written = stored?.type === "api_key" ? (stored.key ?? "") : "";
    expect(["a", "b"]).toContain(written);
  });
});

describe("deleting a credential", () => {
  test("removes it, so a later read finds nothing", async () => {
    const { credentials } = store();
    await credentials.put("openai", apiKey);
    await credentials.delete("openai");
    expect(await credentials.read("openai")).toBeUndefined();
  });

  test("deleting what is not there is not an error", async () => {
    const { credentials } = store();
    await expect(credentials.delete("openai")).resolves.toBeUndefined();
  });
});

describe("the key a credential lives under", () => {
  test("listing strips exactly the prefix, so a provider id is what is left", async () => {
    const { credentials } = store();
    await credentials.put("cloudflare-workers-ai", apiKey);
    expect(await credentials.list()).toEqual([
      { providerId: "cloudflare-workers-ai", type: "api_key" },
    ]);
  });

  test("a credential stored under another key is not listed", async () => {
    const { host, credentials } = store();
    await host.put("linear:credential", { type: "api_key", key: "not-a-provider" });
    expect(await credentials.list()).toEqual([]);
  });
});

describe("the deployment-wide provider object", () => {
  test("writes and reads a credential through its own surface", async () => {
    const object = new ProviderCredentialObject(new Host());
    await object.write("openai", apiKey);
    expect(await object.read("openai")).toEqual(apiKey);
  });

  test("lists and deletes for the status route", async () => {
    const object = new ProviderCredentialObject(new Host());
    await object.write("openai", apiKey);
    expect(await object.list()).toEqual([{ providerId: "openai", type: "api_key" }]);
    await object.delete("openai");
    expect(await object.list()).toEqual([]);
  });

  test("two objects over one storage share the credential, which is the point", async () => {
    const host = new Host();
    await new ProviderCredentialObject(host).write("anthropic", oauth);
    expect(await new ProviderCredentialObject(host).read("anthropic")).toEqual(oauth);
  });
});
