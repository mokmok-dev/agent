import { describe, expect, test } from "bun:test";
import type { TokenRequest, TokenResponse } from "@agent/adapters";
import { LinearOAuthObject, stateKey } from "./linear-oauth-do.ts";
import { finishLogin, startLogin, type LinearOAuthPort } from "./linear-oauth-port.ts";
import { oauthConfigOf } from "./routes.ts";
import type { StorageHost } from "./storage.ts";
import { envWith, OAUTH } from "./test-env.ts";

/**
 * The real object, driven through the real routes. A fake port cannot hide a
 * disagreement between the state in the URL and the state in storage, which is
 * the bug this file exists for.
 */
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

/** A token endpoint that answers one pair, and records what it was asked. */
function endpoint(): { request: TokenRequest; forms: string[] } {
  const forms: string[] = [];
  return {
    forms,
    request: async (_url, form) => {
      forms.push(form);
      return {
        access_token: "at-1",
        refresh_token: "rt-1",
        expires_in: 86_400,
        scope: "read write",
      } satisfies TokenResponse;
    },
  };
}

function flow() {
  const host = new Host();
  const { request, forms } = endpoint();
  const port = new LinearOAuthObject(host, oauthConfigOf(OAUTH), request);
  return { host, port: port as unknown as LinearOAuthPort, forms };
}

describe("the login end to end, over the real object", () => {
  test("the state Linear sends back is the one the object stored", async () => {
    const { host, port } = flow();
    const response = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const location = new URL(response.headers.get("location") ?? "");
    const sent = location.searchParams.get("state") ?? "";
    expect(sent).not.toBe("");
    expect(await host.get<string>(stateKey())).toBe(sent);
  });

  test("a callback carrying the state from the URL completes the login", async () => {
    const { port, forms } = flow();
    const start = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const sent = new URL(start.headers.get("location") ?? "").searchParams.get("state") ?? "";
    const response = await finishLogin(port, { code: "code-1", state: sent }, 10);
    expect(response.status).toBe(200);
    expect(await response.text()).toContain("Linear connected");
    expect(forms).toHaveLength(1);
  });

  test("a callback with any other state is refused and exchanges nothing", async () => {
    const { port, forms } = flow();
    await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const response = await finishLogin(port, { code: "code-1", state: "not-the-state" }, 10);
    expect(response.status).toBe(400);
    expect(forms).toHaveLength(0);
  });

  test("after a success, the same callback is refused, so a replay cannot log in twice", async () => {
    const { port, forms } = flow();
    const start = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const sent = new URL(start.headers.get("location") ?? "").searchParams.get("state") ?? "";
    await finishLogin(port, { code: "code-1", state: sent }, 10);
    const replay = await finishLogin(port, { code: "code-1", state: sent }, 11);
    expect(replay.status).toBe(400);
    expect(forms).toHaveLength(1);
  });

  test("two logins in a row use two different states, and only the second is valid", async () => {
    const { port } = flow();
    const first = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const second = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const firstState = new URL(first.headers.get("location") ?? "").searchParams.get("state") ?? "";
    const secondState =
      new URL(second.headers.get("location") ?? "").searchParams.get("state") ?? "";
    expect(firstState).not.toBe(secondState);
    expect((await finishLogin(port, { code: "c", state: firstState }, 10)).status).toBe(400);
    expect((await finishLogin(port, { code: "c", state: secondState }, 10)).status).toBe(200);
  });
});
