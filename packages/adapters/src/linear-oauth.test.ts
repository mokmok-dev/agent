import { describe, expect, test } from "bun:test";
import {
  AUTHORIZE_URL,
  authorizeUrl,
  callbackOf,
  bearer,
  DEFAULT_ACTOR,
  DEFAULT_SCOPES,
  exchangeCode,
  exchangeForm,
  needsRefresh,
  newState,
  readCallback,
  refreshForm,
  REFRESH_SKEW_MS,
  refreshTokens,
  REVOKE_URL,
  TOKEN_URL,
  tokensOf,
  type LinearOAuthConfig,
  type LinearTokens,
  type TokenResponse,
} from "./linear-oauth.ts";
import { AdapterError } from "./events.ts";

const config: LinearOAuthConfig = {
  clientId: "client-1",
  clientSecret: "secret-1",
  redirectUri: "https://example.test/oauth/linear/callback",
};

const tokenResponse = (overrides: TokenResponse = {}): TokenResponse => ({
  access_token: "at-1",
  refresh_token: "rt-1",
  expires_in: 86_400,
  scope: "read write",
  ...overrides,
});

describe("the constants Linear's contract fixes", () => {
  test("the endpoints are Linear's own", () => {
    expect(AUTHORIZE_URL).toBe("https://linear.app/oauth/authorize");
    expect(TOKEN_URL).toBe("https://api.linear.app/oauth/token");
    expect(REVOKE_URL).toBe("https://api.linear.app/oauth/revoke");
  });

  test("the default scopes read the workspace and let the app act", () => {
    expect(DEFAULT_SCOPES).toEqual(["read", "write", "app:mentionable", "app:assignable"]);
  });

  test("the bearer header is what the GraphQL API reads", () => {
    const tokens: LinearTokens = {
      accessToken: "at-9",
      refreshToken: "rt",
      expiresIn: 1,
      issuedAt: 0,
      scope: "",
    };
    expect(bearer(tokens)).toEqual({ authorization: "Bearer at-9" });
  });
});

describe("the consent URL", () => {
  test("carries every parameter Linear requires", () => {
    const url = new URL(authorizeUrl(config, "state-1"));
    expect(url.origin + url.pathname).toBe("https://linear.app/oauth/authorize");
    expect(url.searchParams.get("response_type")).toBe("code");
    expect(url.searchParams.get("client_id")).toBe("client-1");
    expect(url.searchParams.get("redirect_uri")).toBe(config.redirectUri);
    expect(url.searchParams.get("state")).toBe("state-1");
  });

  test("installs as the app, because the default scopes are app-only", () => {
    const url = new URL(authorizeUrl(config, "s"));
    expect(url.searchParams.get("actor")).toBe("app");
    expect(DEFAULT_ACTOR).toBe("app");
  });

  test("a user actor is available for a deployment that only needs user scopes", () => {
    const url = new URL(authorizeUrl({ ...config, actor: "user", scopes: ["read"] }, "s"));
    expect(url.searchParams.get("actor")).toBe("user");
    expect(url.searchParams.get("scope")).toBe("read");
  });

  test("asks for the documented scopes by default, comma separated", () => {
    const url = new URL(authorizeUrl(config, "s"));
    expect(url.searchParams.get("scope")).toBe(DEFAULT_SCOPES.join(","));
  });

  test("a caller can narrow the scopes", () => {
    const url = new URL(authorizeUrl({ ...config, scopes: ["read"] }, "s"));
    expect(url.searchParams.get("scope")).toBe("read");
  });

  test("escapes the redirect URI and the state", () => {
    const url = new URL(authorizeUrl({ ...config, redirectUri: "https://x/a b?c=d" }, "s&t"));
    expect(url.searchParams.get("redirect_uri")).toBe("https://x/a b?c=d");
    expect(url.searchParams.get("state")).toBe("s&t");
  });
});

describe("the state", () => {
  test("is long, hex, and different every time", () => {
    const first = newState();
    const second = newState();
    expect(first).toMatch(/^[0-9a-f]{64}$/);
    expect(first).not.toBe(second);
  });
});

describe("the callback query", () => {
  test("becomes a plain record, so it crosses an RPC boundary", () => {
    const query = new URLSearchParams({ code: "c1", state: "s1" });
    expect(callbackOf(query)).toEqual({ code: "c1", state: "s1" });
  });

  test("keeps an empty query empty", () => {
    expect(callbackOf(new URLSearchParams())).toEqual({});
  });

  test("keeps a repeated key's last value, which is the one the platform would read", () => {
    expect(callbackOf(new URLSearchParams("code=first&code=last"))).toEqual({ code: "last" });
  });
});

describe("the callback", () => {
  const params = (values: Record<string, string>) => values;

  test("reads a code and a matching state", () => {
    expect(readCallback(params({ code: "c1", state: "s1" }), "s1")).toEqual({
      code: "c1",
      state: "s1",
    });
  });

  test("a callback whose state matches nothing, even when none was issued, is refused", () => {
    expect(
      caught(() => readCallback(params({ code: "c1", state: "forged" }), "")).message,
    ).toContain("does not match");
  });

  test("a state that equals the expected one is accepted whatever its shape", () => {
    expect(readCallback(params({ code: "c1", state: "x y/z" }), "x y/z").state).toBe("x y/z");
  });

  test("refuses a provider error with Linear's own reason", () => {
    const error = caught(() => readCallback(params({ error: "access_denied" }), "s1"));
    expect(error).toBeInstanceOf(AdapterError);
    expect(error.message).toContain("access_denied");
    expect(error.status).toBe(400);
  });

  test("refuses a missing code", () => {
    expect(caught(() => readCallback(params({ state: "s1" }), "s1")).message).toContain("no code");
  });

  test("refuses an empty code", () => {
    expect(caught(() => readCallback(params({ code: "", state: "s1" }), "s1")).message).toContain(
      "no code",
    );
  });

  test("refuses a missing state", () => {
    expect(caught(() => readCallback(params({ code: "c1" }), "s1")).message).toContain("no state");
  });

  test("refuses a state that differs from the one issued", () => {
    expect(
      caught(() => readCallback(params({ code: "c1", state: "other" }), "s1")).message,
    ).toContain("does not match");
  });

  test("a stray callback with an empty state is refused before the comparison", () => {
    expect(caught(() => readCallback(params({ code: "c1", state: "" }), "")).message).toContain(
      "no state",
    );
  });

  test("refuses a callback whose state matches nothing, even when none was issued", () => {
    expect(
      caught(() => readCallback(params({ code: "c1", state: "forged" }), "")).message,
    ).toContain("does not match");
  });
});

describe("the token forms", () => {
  test("an exchange carries the code, the redirect, and the grant", () => {
    const form = new URLSearchParams(exchangeForm(config, "c1"));
    expect(form.get("code")).toBe("c1");
    expect(form.get("redirect_uri")).toBe(config.redirectUri);
    expect(form.get("client_id")).toBe("client-1");
    expect(form.get("client_secret")).toBe("secret-1");
    expect(form.get("grant_type")).toBe("authorization_code");
  });

  test("a refresh carries the refresh token and the grant", () => {
    const form = new URLSearchParams(refreshForm(config, "rt-1"));
    expect(form.get("refresh_token")).toBe("rt-1");
    expect(form.get("grant_type")).toBe("refresh_token");
    expect(form.get("client_id")).toBe("client-1");
    expect(form.get("client_secret")).toBe("secret-1");
  });
});

describe("reading a token response", () => {
  test("keeps the pair, the lifetime, and the issue time", () => {
    expect(tokensOf(tokenResponse(), 1000)).toEqual({
      accessToken: "at-1",
      refreshToken: "rt-1",
      expiresIn: 86_400,
      issuedAt: 1000,
      scope: "read write",
    });
  });

  test("accepts the older array scope", () => {
    expect(tokensOf(tokenResponse({ scope: ["read", "write"] }), 0).scope).toBe("read write");
  });

  test("a missing scope reads as empty", () => {
    expect(tokensOf({ access_token: "a", refresh_token: "r" }, 0).scope).toBe("");
  });

  test("a missing lifetime takes Linear's 24-hour default", () => {
    expect(tokensOf({ access_token: "a", refresh_token: "r" }, 0).expiresIn).toBe(86_400);
  });

  test("refuses a response that reports an error, quoting Linear", () => {
    const error = caught(() =>
      tokensOf({ error: "invalid_grant", error_description: "code expired" }, 0),
    );
    expect(error.status).toBe(502);
    expect(error.message).toContain("code expired");
  });

  test("an error with no description still names the code", () => {
    expect(caught(() => tokensOf({ error: "invalid_client" }, 0)).message).toContain(
      "invalid_client",
    );
  });

  test("refuses a response without an access token", () => {
    expect(caught(() => tokensOf({ refresh_token: "r" }, 0)).message).toContain("no access token");
  });

  test("refuses a response without a refresh token, which would die in a day", () => {
    expect(caught(() => tokensOf({ access_token: "a" }, 0)).message).toContain("no refresh token");
  });
});

describe("refresh timing", () => {
  const tokens = (overrides: Partial<LinearTokens> = {}): LinearTokens => ({
    accessToken: "at",
    refreshToken: "rt",
    expiresIn: 3600,
    issuedAt: 0,
    scope: "",
    ...overrides,
  });

  test("is not needed early in the life of a token", () => {
    expect(needsRefresh(tokens(), 1000)).toBe(false);
  });

  test("is needed inside the skew before expiry", () => {
    expect(needsRefresh(tokens(), 3600 * 1000 - 60 * 1000)).toBe(true);
  });

  test("the boundary itself counts as needing a refresh, so a request cannot race it", () => {
    const boundary = 3600 * 1000 - REFRESH_SKEW_MS;
    expect(needsRefresh(tokens(), boundary)).toBe(true);
    expect(needsRefresh(tokens(), boundary - 1)).toBe(false);
  });

  test("is needed once the token has expired", () => {
    expect(needsRefresh(tokens(), 7200 * 1000)).toBe(true);
  });
});

describe("the network calls", () => {
  test("an exchange posts the form to the token endpoint and returns the pair", async () => {
    const calls: { url: string; form: string }[] = [];
    const tokens = await exchangeCode(config, "c1", 5, async (url, form) => {
      calls.push({ url, form });
      return tokenResponse();
    });
    expect(calls[0]?.url).toBe("https://api.linear.app/oauth/token");
    expect(new URLSearchParams(calls[0]?.form).get("grant_type")).toBe("authorization_code");
    expect(tokens.accessToken).toBe("at-1");
  });

  test("a refresh posts the refresh grant and returns the new pair", async () => {
    let seen = "";
    const tokens = await refreshTokens(config, "rt-1", 5, async (_url, form) => {
      seen = form;
      return tokenResponse({ access_token: "at-2", refresh_token: "rt-2" });
    });
    expect(new URLSearchParams(seen).get("grant_type")).toBe("refresh_token");
    expect(tokens.refreshToken).toBe("rt-2");
  });
});

function caught(run: () => unknown): AdapterError {
  try {
    run();
  } catch (error) {
    if (error instanceof AdapterError) return error;
    throw error;
  }
  throw new Error("expected a refusal");
}
