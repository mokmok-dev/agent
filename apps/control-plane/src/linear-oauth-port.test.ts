import { describe, expect, test } from "bun:test";
import type { LinearCredential } from "./linear-oauth-do.ts";
import { finishLogin, startLogin, type LinearOAuthPort } from "./linear-oauth-port.ts";
import { oauthConfigOf, oauthConfigured } from "./routes.ts";
import { envWith, OAUTH } from "./test-env.ts";

/** A port that records what the Worker asked for and answers as told. */
class FakePort implements LinearOAuthPort {
  started = 0;
  readonly paramses: Record<string, string>[] = [];
  readonly stored: string[] = [];
  answer: LinearCredential | Error;

  constructor(answer: LinearCredential | Error) {
    this.answer = answer;
  }

  async start(state: string): Promise<void> {
    this.started += 1;
    this.stored.push(state);
  }

  async complete(params: Record<string, string>, _now: number): Promise<LinearCredential> {
    this.paramses.push(params);
    if (this.answer instanceof Error) throw this.answer;
    return this.answer;
  }
}

const credential = (overrides: Partial<LinearCredential> = {}): LinearCredential => ({
  accessToken: "at",
  refreshToken: "rt",
  expiresIn: 86_400,
  issuedAt: 0,
  scope: "read",
  organizationId: "org-1",
  ...overrides,
});

describe("starting the login", () => {
  test("stores the same state it puts in the URL, so the callback cannot disagree", async () => {
    const port = new FakePort(credential());
    const response = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const location = new URL(response.headers.get("location") ?? "");
    expect(port.stored).toEqual([location.searchParams.get("state") ?? ""]);
    expect(port.stored[0]).toMatch(/^[0-9a-f]{64}$/);
  });

  test("stores the pinned state when a caller supplies one", async () => {
    const port = new FakePort(credential());
    const response = await startLogin(port, envWith(), oauthConfigOf(OAUTH), "pinned");
    const location = new URL(response.headers.get("location") ?? "");
    expect(port.stored).toEqual(["pinned"]);
    expect(location.searchParams.get("state")).toBe("pinned");
  });

  test("a stored state is accepted by a later callback, which is the whole flow", async () => {
    const port = new FakePort(credential());
    await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    const stored = port.stored[0] ?? "";
    const response = await finishLogin(port, { code: "c1", state: stored }, 0);
    expect(response.status).toBe(200);
  });

  test("redirects to Linear's consent screen", async () => {
    const port = new FakePort(credential());
    const response = await startLogin(port, envWith(), oauthConfigOf(OAUTH));
    expect(port.started).toBe(1);
    expect(response.status).toBe(302);
    const location = new URL(response.headers.get("location") ?? "");
    expect(location.origin + location.pathname).toBe("https://linear.app/oauth/authorize");
    expect(location.searchParams.get("client_id")).toBe(OAUTH.LINEAR_CLIENT_ID);
    expect(location.searchParams.get("redirect_uri")).toBe(OAUTH.LINEAR_REDIRECT_URI);
    expect(location.searchParams.get("actor")).toBe("app");
  });

  test("uses the state it was given, so a test can pin it", async () => {
    const port = new FakePort(credential());
    const response = await startLogin(port, envWith(), oauthConfigOf(OAUTH), "pinned");
    const location = new URL(response.headers.get("location") ?? "");
    expect(location.searchParams.get("state")).toBe("pinned");
  });

  test("reports a missing client id instead of sending a person to a consent screen that cannot work", async () => {
    const port = new FakePort(credential());
    const env = envWith({ LINEAR_CLIENT_ID: "" });
    const response = await startLogin(port, env, oauthConfigOf(env));
    expect(response.status).toBe(503);
    expect(await response.json()).toEqual({
      error: { message: "LINEAR_CLIENT_ID and LINEAR_CLIENT_SECRET must be set" },
    });
    expect(port.started).toBe(0);
  });

  test("reports a missing client secret the same way", async () => {
    const port = new FakePort(credential());
    const env = envWith({ LINEAR_CLIENT_SECRET: "" });
    expect((await startLogin(port, env, oauthConfigOf(env))).status).toBe(503);
    expect(port.started).toBe(0);
  });
});

describe("the config mapping", () => {
  test("reads the three platform names into the adapter's shape", () => {
    expect(oauthConfigOf(OAUTH)).toEqual({
      clientId: "client-1",
      clientSecret: "secret-1",
      redirectUri: "https://example.test/oauth/linear/callback",
    });
  });

  test("a pair of secrets is configured once both are present", () => {
    expect(oauthConfigured(OAUTH)).toBe(true);
    expect(oauthConfigured({ ...OAUTH, LINEAR_CLIENT_ID: "" })).toBe(false);
    expect(oauthConfigured({ ...OAUTH, LINEAR_CLIENT_SECRET: "" })).toBe(false);
  });
});

describe("finishing the login", () => {
  test("shows the workspace and the token lifetime on success", async () => {
    const port = new FakePort(credential({ organizationId: "mokmok", expiresIn: 3600 }));
    const response = await finishLogin(port, { code: "c", state: "s" }, 10);
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/html");
    const body = await response.text();
    expect(body).toContain("<title>Linear connected</title>");
    expect(body).toContain("<h1>Linear connected</h1>");
    expect(body).toContain("mokmok");
    expect(body).toContain("1 hours");
  });

  test("escapes markup in the page, so a workspace name cannot inject a tag", async () => {
    const port = new FakePort(credential({ organizationId: "<script>alert(1)</script>" }));
    const body = await (await finishLogin(port, {}, 0)).text();
    expect(body).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
    expect(body).not.toContain("<script>alert(1)");
  });

  test("escapes an ampersand", async () => {
    const port = new FakePort(credential({ organizationId: "a & b" }));
    const body = await (await finishLogin(port, {}, 0)).text();
    expect(body).toContain("a &amp; b");
  });

  test("names the workspace generically when Linear has not reported one", async () => {
    const port = new FakePort(credential({ organizationId: null }));
    const body = await (await finishLogin(port, {}, 0)).text();
    expect(body).toContain("the authorized workspace");
  });

  test("passes the callback parameters through to the port", async () => {
    const port = new FakePort(credential());
    const params = { code: "c1", state: "s1" };
    await finishLogin(port, params, 10);
    expect(port.paramses[0]).toEqual(params);
  });

  test("a callback refusal is a 400, whatever the thrown error's own fields", async () => {
    const port = new FakePort(new Error("linear callback state does not match"));
    const response = await finishLogin(port, {}, 10);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({
      error: { message: "linear callback state does not match" },
    });
  });

  test("a bad token response is a 502, because Linear answered something unusable", async () => {
    const port = new FakePort(new Error("linear token response has no refresh token"));
    expect((await finishLogin(port, {}, 0)).status).toBe(502);
  });

  test("an unreachable token endpoint is a 502", async () => {
    const port = new FakePort(new Error("linear token endpoint unreachable: offline"));
    expect((await finishLogin(port, {}, 0)).status).toBe(502);
  });

  test("an unrecognized failure is a 400, because the object only refuses callbacks", async () => {
    const response = await finishLogin(new FakePort(new Error("boom")), {}, 0);
    expect(response.status).toBe(400);
  });

  test("a thrown non-error still answers with its text", async () => {
    const port: LinearOAuthPort = {
      start: async () => undefined,
      complete: async () => {
        throw "plain string";
      },
    };
    const response = await finishLogin(port, {}, 0);
    expect(await response.json()).toEqual({ error: { message: "plain string" } });
  });
});
