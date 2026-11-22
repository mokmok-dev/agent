/**
 * Why a Linear OAuth step did not complete. `rejected` is a callback or a
 * configuration this module will not accept; `unreachable` is a transport that
 * failed; `bad-response` is an answer it cannot use.
 */
export type OAuthRefused =
  | { kind: "rejected"; status: number; message: string }
  | { kind: "unreachable"; message: string }
  | { kind: "bad-response"; message: string };

export type TokenOutcome = { kind: "tokens"; tokens: LinearTokens } | OAuthRefused;

export type CallbackOutcome = { kind: "callback"; params: CallbackParams } | OAuthRefused;

export type ViewerOutcome = { kind: "viewer"; viewer: Viewer } | OAuthRefused;

export type Revoked = { kind: "revoked"; revoked: boolean } | OAuthRefused;

/**
 * Linear's OAuth 2.0 authorization-code flow. Three rules matter here and each
 * is data:
 *
 * - `state` is a random value stored on the object that starts the flow. It is
 *   compared on the callback, so a callback nobody started is refused.
 * - The token request is a form body, which is what Linear reads.
 * - An access token lives 24 hours and a refresh token 30 days, so the stored
 *   credentials keep `expires_in` and are refreshed before use.
 *
 * Every function returns a `Result`. The network transport is the only place a
 * throw can start, so it is the only place that catches one.
 */

export const AUTHORIZE_URL = "https://linear.app/oauth/authorize";
export const TOKEN_URL = "https://api.linear.app/oauth/token";
export const REVOKE_URL = "https://api.linear.app/oauth/revoke";

/** The scopes this deployment asks for: read the workspace, and write agent activity. */
export const DEFAULT_SCOPES = ["read", "write", "app:mentionable", "app:assignable"] as const;

/**
 * Whose identity the application acts as. An agent installs as the app, which
 * is also what makes the app-only scopes above legal; a user actor with an
 * app-only scope is refused by Linear.
 */
export type LinearActor = "app" | "user";

export const DEFAULT_ACTOR: LinearActor = "app";

/** The token an authorization returns, plus when it was issued. */
export type LinearTokens = {
  accessToken: string;
  refreshToken: string;
  /** Seconds the access token lives, as Linear reports it. */
  expiresIn: number;
  /** Epoch milliseconds when the token response arrived. */
  issuedAt: number;
  scope: string;
};

export type LinearOAuthConfig = {
  clientId: string;
  clientSecret: string;
  /** The exact redirect URI registered on the Linear application. */
  redirectUri: string;
  scopes?: readonly string[];
  /**
   * Whose identity the token carries. Defaults to `app`, because the default
   * scopes include app-only ones; use `user` only with user-only scopes.
   */
  actor?: LinearActor;
};

/**
 * The URL a person is sent to. `state` is supplied by the caller and stored
 * before this call, so the callback can prove it started the flow.
 */
export function authorizeUrl(config: LinearOAuthConfig, state: string): string {
  const url = new URL(AUTHORIZE_URL);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("client_id", config.clientId);
  url.searchParams.set("redirect_uri", config.redirectUri);
  url.searchParams.set("scope", (config.scopes ?? DEFAULT_SCOPES).join(","));
  url.searchParams.set("actor", config.actor ?? DEFAULT_ACTOR);
  url.searchParams.set("state", state);
  return url.toString();
}

/** A random state value, from the platform's crypto so it is unpredictable. */
export function newState(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

export type CallbackParams = { code: string; state: string };

/**
 * The callback's query as a plain object. A `URLSearchParams` does not cross an
 * RPC boundary, so the handler reads the query into a record before it reaches
 * the object that owns the state.
 */
export function callbackOf(query: URLSearchParams): Record<string, string> {
  const fields: Record<string, string> = {};
  for (const [key, value] of query) fields[key] = value;
  return fields;
}

/**
 * Read the callback, or refuse it. A provider error, a missing code, and a
 * `state` that does not match the one this object issued are three different
 * refusals, so an operator can tell them apart.
 */
export function readCallback(
  params: Record<string, string>,
  expectedState: string,
): CallbackOutcome {
  const rejected = (message: string): OAuthRefused => ({ kind: "rejected", status: 400, message });
  const error = params["error"];
  if (error !== undefined) return rejected(`linear refused the authorization: ${error}`);
  const code = params["code"];
  if (code === undefined || code === "") return rejected("linear callback has no code");
  const state = params["state"];
  if (state === undefined || state === "") return rejected("linear callback has no state");
  if (state !== expectedState) return rejected("linear callback state does not match");
  return { kind: "callback", params: { code, state } };
}

/** The form body a token request needs. Linear reads `application/x-www-form-urlencoded`. */
export function tokenForm(fields: Readonly<Record<string, string>>): string {
  return new URLSearchParams(fields).toString();
}

/** The token request that trades an authorization code for a token pair. */
export function exchangeForm(config: LinearOAuthConfig, code: string): string {
  return tokenForm({
    code,
    redirect_uri: config.redirectUri,
    client_id: config.clientId,
    client_secret: config.clientSecret,
    grant_type: "authorization_code",
  });
}

/** The token request that trades a refresh token for a new pair. */
export function refreshForm(config: LinearOAuthConfig, refreshToken: string): string {
  return tokenForm({
    refresh_token: refreshToken,
    grant_type: "refresh_token",
    client_id: config.clientId,
    client_secret: config.clientSecret,
  });
}

export type TokenResponse = {
  access_token?: string;
  refresh_token?: string;
  expires_in?: number;
  scope?: string | string[];
  error?: string;
  error_description?: string;
};

/**
 * The tokens from a successful response, or a refusal that names Linear's own
 * error. A response without a refresh token cannot be renewed, so it is refused
 * rather than stored as a credential that dies in a day.
 */
export function tokensOf(response: TokenResponse, now: number): TokenOutcome {
  if (response.error !== undefined) {
    const detail = response.error_description ?? response.error;
    return { kind: "bad-response", message: `linear token request failed: ${detail}` };
  }
  const accessToken = response.access_token;
  const refreshToken = response.refresh_token;
  if (accessToken === undefined) {
    return { kind: "bad-response", message: "linear token response has no access token" };
  }
  if (refreshToken === undefined) {
    return { kind: "bad-response", message: "linear token response has no refresh token" };
  }
  return {
    kind: "tokens",
    tokens: {
      accessToken,
      refreshToken,
      expiresIn: response.expires_in ?? 86_400,
      issuedAt: now,
      scope: scopeOf(response.scope),
    },
  };
}

function scopeOf(scope: string | string[] | undefined): string {
  if (scope === undefined) return "";
  return Array.isArray(scope) ? scope.join(" ") : scope;
}

/** The skew before expiry at which a token is refreshed, so a request never races it. */
export const REFRESH_SKEW_MS = 5 * 60 * 1000;

/**
 * Whether a stored token needs refreshing. Tokens can be revoked early, so a
 * 401 on a request is the other signal; this only covers the documented expiry.
 */
export function needsRefresh(tokens: LinearTokens, now: number): boolean {
  return now >= tokens.issuedAt + tokens.expiresIn * 1000 - REFRESH_SKEW_MS;
}

/** The headers an authenticated Linear GraphQL request carries. */
export function bearer(tokens: LinearTokens): Record<string, string> {
  return { authorization: `Bearer ${tokens.accessToken}` };
}

export type TokenRequest = (url: string, form: string) => Promise<TokenBody>;

/** A token endpoint's answer, before it is read as tokens. */
export type TokenBody = { kind: "body"; body: TokenResponse } | OAuthRefused;

/** Trade an authorization code for tokens over the network. */
export async function exchangeCode(
  config: LinearOAuthConfig,
  code: string,
  now: number,
  request: TokenRequest = postForm,
): Promise<TokenOutcome> {
  const answer = await request(TOKEN_URL, exchangeForm(config, code));
  return answer.kind === "body" ? tokensOf(answer.body, now) : answer;
}

/** Refresh a token pair over the network. */
export async function refreshTokens(
  config: LinearOAuthConfig,
  refreshToken: string,
  now: number,
  request: TokenRequest = postForm,
): Promise<TokenOutcome> {
  const answer = await request(TOKEN_URL, refreshForm(config, refreshToken));
  return answer.kind === "body" ? tokensOf(answer.body, now) : answer;
}

/**
 * The default transport. Linear answers a JSON body, and a non-JSON body or a
 * network failure is a refusal rather than an exception escaping the handler.
 * This is the one place in the adapters that catches a throw.
 */
export async function postForm(url: string, form: string): Promise<TokenBody> {
  const text = await postText(url, form);
  return text.kind === "text" ? parseTokenBody(text.text, text.status) : text;
}

type PostedText = { kind: "text"; text: string; status: number } | OAuthRefused;

async function postText(url: string, form: string): Promise<PostedText> {
  try {
    const response = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: form,
    });
    return { kind: "text", text: await response.text(), status: response.status };
  } catch (error) {
    return { kind: "unreachable", message: `linear token endpoint unreachable: ${String(error)}` };
  }
}

function parseTokenBody(text: string, status: number): TokenBody {
  let body: TokenResponse;
  try {
    body = JSON.parse(text) as TokenResponse;
  } catch {
    return { kind: "bad-response", message: `linear token endpoint answered ${status}` };
  }
  if (status < 200 || status >= 300) {
    return body.error === undefined
      ? { kind: "bad-response", message: `linear token endpoint answered ${status}` }
      : { kind: "body", body };
  }
  return { kind: "body", body };
}

/** Revoke a token, best effort: a failure is reported, not thrown. */
export async function revokeToken(
  token: string,
  request: TokenRequest = postForm,
): Promise<Revoked> {
  const answer = await request(REVOKE_URL, tokenForm({ token }));
  return answer.kind === "body"
    ? { kind: "revoked", revoked: answer.body.error === undefined }
    : answer;
}

export const GRAPHQL_URL = "https://api.linear.app/graphql";

/** A GraphQL call, as a port so a test answers without the network. */
export type GraphqlRequest = (
  url: string,
  init: { method: string; headers: Record<string, string>; body: string },
) => Promise<GraphqlBody>;

/** A GraphQL answer, before it is read as a value. */
export type GraphqlBody = { kind: "body"; body: unknown } | OAuthRefused;

export type Viewer = { id: string };

/**
 * The app's id in the workspace that authorized it. Linear documents this as the
 * value to store with the token, because it is unique per workspace.
 */
export function parseViewer(body: unknown): ViewerOutcome {
  const viewer = (body as { data?: { viewer?: { id?: unknown } } } | undefined)?.data?.viewer;
  const id = viewer?.id;
  return typeof id === "string" && id !== ""
    ? { kind: "viewer", viewer: { id } }
    : { kind: "bad-response", message: "linear answered without a viewer" };
}

/**
 * Ask Linear which app user the token belongs to. A failure is a refusal the
 * caller may ignore, because a login that already stored a usable token should
 * not be reported as failed for want of an id.
 */
export async function fetchViewer(
  accessToken: string,
  request: GraphqlRequest = postGraphql,
): Promise<ViewerOutcome> {
  const answer = await request(GRAPHQL_URL, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${accessToken}` },
    body: JSON.stringify({ query: "{ viewer { id name } }" }),
  });
  return answer.kind === "body" ? parseViewer(answer.body) : answer;
}

/** The default GraphQL transport. A network failure or a non-JSON body is a refusal. */
export async function postGraphql(
  url: string,
  init: { method: string; headers: Record<string, string>; body: string },
): Promise<GraphqlBody> {
  try {
    const response = await fetch(url, init);
    const text = await response.text();
    try {
      return { kind: "body", body: JSON.parse(text) as unknown };
    } catch {
      return { kind: "bad-response", message: `linear graphql answered ${response.status}` };
    }
  } catch (error) {
    return { kind: "unreachable", message: `linear graphql unreachable: ${String(error)}` };
  }
}
