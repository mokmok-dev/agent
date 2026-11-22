import { describe, expect, test } from "bun:test";
import type { GraphqlRequest, TokenRequest, TokenResponse } from "@agent/adapters";
import {
  credentialKey,
  expectedState,
  LinearOAuthObject,
  stateKey,
  type AccessToken,
  type Completed,
  type LinearCredential,
  type LoginRefused,
} from "./linear-oauth-do.ts";
import type { StorageHost } from "./storage.ts";

describe("reading a stored state", () => {
  test("a string is the state", () => {
    expect(expectedState("abc")).toBe("abc");
  });

  test("anything that is not a string reads as no state, so a corrupted entry cannot log in", () => {
    for (const value of [undefined, null, 42, { a: 1 }, ["x"]]) {
      expect(expectedState(value)).toBe("");
    }
  });
});

/** An in-memory host, so the object is exercised over its real storage calls. */
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

/** The token a renew produced, or a failure naming the refusal. */
function tokenOf(answer: AccessToken | undefined): string {
  if (answer === undefined) throw new Error("expected a credential");
  if (answer.kind !== "token") throw new Error(`expected a token: `);
  return answer.token;
}

/** The credential a login produced, or a failure naming the refusal. */
function credentialOf(answer: Completed): LinearCredential {
  if (answer.kind !== "completed") throw new Error(`expected a credential: ${answer.message}`);
  return answer.credential;
}

const config = {
  clientId: "client-1",
  clientSecret: "secret-1",
  redirectUri: "https://example.test/oauth/linear/callback",
};

const answer = (overrides: TokenResponse = {}): TokenResponse => ({
  access_token: "at-1",
  refresh_token: "rt-1",
  expires_in: 86_400,
  scope: "read write",
  ...overrides,
});

/** A token endpoint that records each form and answers in order. */
function endpoint(answers: TokenResponse[]): { request: TokenRequest; forms: string[] } {
  const forms: string[] = [];
  const request: TokenRequest = async (_url, form) => {
    forms.push(form);
    return { kind: "body", body: answers.shift() ?? answer() };
  };
  return { request, forms };
}

const object = (
  request: TokenRequest = endpoint([answer()]).request,
  graphql: GraphqlRequest = async () => ({ kind: "body", body: {} }),
) => {
  const host = new Host();
  return { host, oauth: new LinearOAuthObject(host, config, request, graphql) };
};

describe("starting a login", () => {
  test("writes the state under one fixed key, so one deployment keeps one login at a time", async () => {
    const { host, oauth } = object();
    await oauth.start("state-1");
    expect([...host.values.keys()]).toEqual(["linear:state"]);
  });

  test("stores the credential under one fixed key", async () => {
    const { host, oauth } = object();
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    expect([...host.values.keys()].sort()).toEqual(["linear:credential"]);
  });

  test("stores exactly the state the caller put in the consent URL", async () => {
    const { host, oauth } = object();
    await oauth.start("state-1");
    expect(await host.get<string>(stateKey())).toBe("state-1");
  });

  test("verifies only the state it stored", async () => {
    const { oauth } = object();
    await oauth.start("state-1");
    expect(await oauth.verify("state-1")).toBe(true);
    expect(await oauth.verify("other")).toBe(false);
  });

  test("verifies nothing before a login starts", async () => {
    const { oauth } = object();
    expect(await oauth.verify("anything")).toBe(false);
  });

  test("verifies nothing against a state the storage holds as a non-string", async () => {
    const { host, oauth } = object();
    await host.put(stateKey(), { not: "a string" });
    expect(await oauth.verify("[object Object]")).toBe(false);
  });

  test("verifies nothing after the stored state is blank", async () => {
    const { host, oauth } = object();
    await oauth.start("");
    await host.put(stateKey(), "");
    expect(await oauth.verify("")).toBe(false);
  });

  test("verifies a state that a previous login left behind, which is not blank", async () => {
    const { host, oauth } = object();
    await host.put(stateKey(), "left-over");
    expect(await oauth.verify("left-over")).toBe(true);
  });
});

describe("completing a login", () => {
  test("exchanges the code and stores the pair", async () => {
    const { oauth } = object();
    await oauth.start("state-1");
    const credential = credentialOf(await oauth.complete({ code: "c1", state: "state-1" }, 10));
    expect(credential).toMatchObject({
      accessToken: "at-1",
      refreshToken: "rt-1",
      organizationId: null,
    });
    expect(await oauth.credential()).toEqual(credential);
  });

  test("stores the app's id in the workspace, which Linear says to keep with the token", async () => {
    const { host, oauth } = object(undefined, async () => ({
      kind: "body",
      body: { data: { viewer: { id: "app-1" } } },
    }));
    await oauth.start("s");
    const credential = credentialOf(await oauth.complete({ code: "c", state: "s" }, 10));
    expect(credential.organizationId).toBe("app-1");
    expect((await oauth.credential())?.organizationId).toBe("app-1");
    expect(host.values.size).toBe(1);
  });

  test("asks Linear for the viewer with the token it just stored", async () => {
    const seen: { url: string; auth: string | undefined }[] = [];
    const { oauth } = object(undefined, async (url, init) => {
      seen.push({ url, auth: init.headers["authorization"] });
      return { kind: "body", body: { data: { viewer: { id: "app-1" } } } };
    });
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 10);
    expect(seen).toEqual([{ url: "https://api.linear.app/graphql", auth: "Bearer at-1" }]);
  });

  test("keeps the credential when the viewer lookup fails, because the token still works", async () => {
    const { oauth } = object(undefined, async () => ({
      kind: "unreachable",
      message: "offline",
    }));
    await oauth.start("s");
    const credential = credentialOf(await oauth.complete({ code: "c", state: "s" }, 10));
    expect(credential.accessToken).toBe("at-1");
    expect(credential.organizationId).toBeNull();
    expect(await oauth.credential()).toBeDefined();
  });

  test("keeps the credential when Linear answers without a viewer id", async () => {
    const { oauth } = object(undefined, async () => ({ kind: "body", body: { data: {} } }));
    await oauth.start("s");
    expect(
      credentialOf(await oauth.complete({ code: "c", state: "s" }, 10)).organizationId,
    ).toBeNull();
  });

  test("consumes the state, so a replayed callback is refused", async () => {
    const { oauth } = object();
    await oauth.start("state-1");
    await oauth.complete({ code: "c1", state: "state-1" }, 10);
    const replay = await refusalOf(() => oauth.complete({ code: "c1", state: "state-1" }, 11));
    expect(replay.message).toContain("does not match");
  });

  test("exchanges nothing when the state is wrong, so a forged callback costs no network call", async () => {
    const { forms, request } = endpoint([answer()]);
    const { oauth } = object(request);
    await oauth.start("state-1");
    const refusal = await refusalOf(() => oauth.complete({ code: "c1", state: "forged" }, 10));
    expect(refusal.message).toContain("does not match");
    expect(forms).toEqual([]);
  });

  test("consumes the state before exchanging, so a failed exchange needs a fresh login", async () => {
    const { request } = endpoint([{ error: "invalid_grant" }]);
    const { host, oauth } = object(request);
    await oauth.start("state-1");
    await oauth.complete({ code: "c1", state: "state-1" }, 10).catch(() => undefined);
    expect(await host.get<string>(stateKey())).toBeUndefined();
    expect(await oauth.credential()).toBeUndefined();
  });
});

describe("using the credential", () => {
  test("returns no token before a login", async () => {
    const { oauth } = object();
    expect(await oauth.accessToken(0)).toBeUndefined();
  });

  test("returns the stored token while it is fresh", async () => {
    const { forms, request } = endpoint([answer(), answer({ access_token: "at-2" })]);
    const { oauth } = object(request);
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    expect(tokenOf(await oauth.accessToken(1000))).toBe("at-1");
    expect(forms).toHaveLength(1);
  });

  test("renews inside the refresh skew and stores the new pair", async () => {
    const { forms, request } = endpoint([
      answer(),
      answer({ access_token: "at-2", refresh_token: "rt-2" }),
    ]);
    const { oauth } = object(request);
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    const token = tokenOf(await oauth.accessToken(86_400 * 1000));
    expect(token).toBe("at-2");
    expect(new URLSearchParams(forms[1]).get("grant_type")).toBe("refresh_token");
    expect((await oauth.credential())?.refreshToken).toBe("rt-2");
  });

  test("a forced refresh renews a fresh pair too", async () => {
    const { request } = endpoint([answer(), answer({ access_token: "at-forced" })]);
    const { oauth } = object(request);
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    expect(await oauth.refresh(1)).toBe(true);
    expect((await oauth.credential())?.accessToken).toBe("at-forced");
  });

  test("a forced refresh with no credential reports false", async () => {
    const { oauth } = object();
    expect(await oauth.refresh(1)).toBe(false);
  });

  test("a forced refresh with no credential calls nothing over the network", async () => {
    const { forms, request } = endpoint([answer()]);
    const { oauth } = object(request);
    await oauth.refresh(1);
    expect(forms).toEqual([]);
  });

  test("a renewal keeps the workspace that was recorded", async () => {
    const { request } = endpoint([answer(), answer({ access_token: "at-2" })]);
    const { host, oauth } = object(request);
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    await host.put<LinearCredential>(credentialKey(), {
      ...(await oauth.credential())!,
      organizationId: "org-1",
    });
    await oauth.refresh(1);
    expect((await oauth.credential())?.organizationId).toBe("org-1");
  });
});

describe("disconnecting", () => {
  test("removes the stored pair, so later requests find no token", async () => {
    const { oauth } = object();
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    await oauth.disconnect();
    expect(await oauth.credential()).toBeUndefined();
    expect(await oauth.accessToken(0)).toBeUndefined();
  });

  test("completes when there is nothing to revoke", async () => {
    const { oauth } = object();
    await oauth.disconnect();
    expect(await oauth.credential()).toBeUndefined();
  });

  test("disconnect with no credential calls nothing over the network", async () => {
    const seen: string[] = [];
    const { oauth } = object(async (url) => {
      seen.push(url);
      return { kind: "body", body: {} };
    });
    await oauth.disconnect();
    expect(seen).toEqual([]);
  });

  test("disconnect revokes the stored access token", async () => {
    const forms: string[] = [];
    const { oauth } = object(async (_url, form) => {
      forms.push(form);
      return { kind: "body", body: answer() };
    });
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    await oauth.disconnect();
    expect(new URLSearchParams(forms.at(-1)).get("token")).toBe("at-1");
  });
});

/** The refusal a login returned, so a test asserts on its message. */
async function refusalOf(run: () => Promise<Completed>): Promise<LoginRefused> {
  const answer = await run();
  if (answer.kind === "completed") throw new Error("expected a refusal");
  return answer;
}

describe("a renewal that Linear refused", () => {
  /** A transport whose first call succeeds and whose later calls are unreachable. */
  const failsAfterLogin = (): TokenRequest => {
    let calls = 0;
    return async () => {
      calls += 1;
      return calls === 1
        ? { kind: "body", body: answer() }
        : { kind: "unreachable", message: "offline" };
    };
  };

  test("a failed refresh is reported as the refusal, so the caller can answer it", async () => {
    const { oauth } = object(failsAfterLogin());
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    expect(await oauth.accessToken(86_400 * 1000)).toEqual({
      kind: "unreachable",
      message: "offline",
    });
  });

  test("a failed forced refresh reports false rather than claiming success", async () => {
    const { oauth } = object(failsAfterLogin());
    await oauth.start("s");
    await oauth.complete({ code: "c", state: "s" }, 0);
    expect(await oauth.refresh(1)).toBe(false);
  });
});
