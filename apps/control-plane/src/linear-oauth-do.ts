import {
  exchangeCode,
  fetchViewer,
  needsRefresh,
  postForm,
  postGraphql,
  readCallback,
  refreshTokens,
  revokeToken,
  type LinearTokens,
  type LinearOAuthConfig,
  type GraphqlRequest,
  type TokenRequest,
} from "@agent/adapters";
import type { StorageHost } from "./storage.ts";

export type LinearCredential = LinearTokens & {
  /** The workspace the token belongs to, once a request reports it. */
  organizationId: string | null;
};

/**
 * Why a login or a renewal did not finish. `rejected` is a callback this object
 * will not accept; `unreachable` and `bad-response` are Linear's answers.
 */
export type LoginRefused =
  | { kind: "rejected"; status: number; message: string }
  | { kind: "unreachable"; message: string }
  | { kind: "bad-response"; message: string };

/** The credential a login produced, or the refusal that ended it. */
export type Completed = { kind: "completed"; credential: LinearCredential } | LoginRefused;

/** A usable access token, or the refusal that renewal hit. */
export type AccessToken = { kind: "token"; token: string } | LoginRefused;

export const stateKey = (): string => "linear:state";
export const credentialKey = (): string => "linear:credential";

/**
 * The state a callback is checked against. The storage is untyped, so a value
 * that is not a string reads as no state at all rather than throwing on the
 * comparison.
 */
export function expectedState(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/**
 * The Linear authorization for this deployment. It owns three things:
 *
 * - the `state` a login started with, so a callback nobody started is refused;
 * - the token pair, refreshed before it expires;
 * - the workspace the pair belongs to.
 *
 * There is one object, named `linear`, so a second login replaces the first and
 * an operator can see which workspace the deployment speaks for.
 */
export class LinearOAuthObject {
  readonly #storage: StorageHost;
  readonly #config: LinearOAuthConfig;
  readonly #request: TokenRequest;
  readonly #graphql: GraphqlRequest;

  constructor(
    storage: StorageHost,
    config: LinearOAuthConfig,
    request: TokenRequest = postForm,
    graphql: GraphqlRequest = postGraphql,
  ) {
    this.#storage = storage;
    this.#config = config;
    this.#request = request;
    this.#graphql = graphql;
  }

  /**
   * Store the state the caller put in the consent URL. The caller owns the
   * value, so the URL and the storage cannot disagree.
   */
  async start(state: string): Promise<void> {
    await this.#storage.put(stateKey(), state);
  }

  /**
   * Whether a callback's state is the one this object issued. A stored value
   * that is not a string cannot match, so a corrupted entry reads as no login.
   */
  async verify(state: string): Promise<boolean> {
    const expected = expectedState(await this.#storage.get<unknown>(stateKey()));
    return expected !== "" && expected === state;
  }

  /**
   * Finish a login. The state is consumed first, so a replayed callback is
   * refused even when the code exchange would otherwise succeed twice. The
   * workspace id is fetched after the tokens are stored, so a slow or failing
   * GraphQL call cannot lose a credential that already works.
   *
   * The result crosses a Durable Object RPC, so a refusal is a value rather
   * than a thrown error whose fields the boundary would drop.
   */
  async complete(params: Record<string, string>, now: number): Promise<Completed> {
    const expected = expectedState(await this.#storage.get<unknown>(stateKey()));
    const callback = readCallback(params, expected);
    if (callback.kind !== "callback") return callback;
    await this.#storage.delete(stateKey());
    const tokens = await exchangeCode(this.#config, callback.params.code, now, this.#request);
    if (tokens.kind !== "tokens") return tokens;
    const stored = await this.#store({ ...tokens.tokens, organizationId: null });
    const viewer = await fetchViewer(stored.accessToken, this.#graphql);
    if (viewer.kind !== "viewer") return { kind: "completed", credential: stored };
    return {
      kind: "completed",
      credential: await this.#store({ ...stored, organizationId: viewer.viewer.id }),
    };
  }

  /** The stored credential, or `undefined` before the first login. */
  credential(): Promise<LinearCredential | undefined> {
    return this.#storage.get<LinearCredential>(credentialKey());
  }

  /**
   * A usable access token. A pair inside the refresh skew is renewed first, and
   * the renewed pair replaces the stored one. `undefined` means no login exists.
   */
  async accessToken(now: number): Promise<AccessToken | undefined> {
    const credential = await this.credential();
    if (credential === undefined) return undefined;
    if (!needsRefresh(credential, now)) return { kind: "token", token: credential.accessToken };
    const renewed = await this.#renew(credential, now);
    return renewed.kind === "completed"
      ? { kind: "token", token: renewed.credential.accessToken }
      : renewed;
  }

  /** Refresh now, whatever the expiry says. A 401 on a request calls this. */
  async refresh(now: number): Promise<boolean> {
    const credential = await this.credential();
    if (credential === undefined) return false;
    return (await this.#renew(credential, now)).kind === "completed";
  }

  /** Give up the authorization. The stored pair is removed whether or not Linear answers. */
  async disconnect(): Promise<void> {
    const credential = await this.credential();
    if (credential !== undefined) await revokeToken(credential.accessToken, this.#request);
    await this.#storage.delete(credentialKey());
  }

  async #renew(credential: LinearCredential, now: number): Promise<Completed> {
    const renewed = await refreshTokens(this.#config, credential.refreshToken, now, this.#request);
    if (renewed.kind !== "tokens") return renewed;
    return {
      kind: "completed",
      credential: await this.#store({
        ...renewed.tokens,
        organizationId: credential.organizationId,
      }),
    };
  }

  async #store(credential: LinearCredential): Promise<LinearCredential> {
    await this.#storage.put(credentialKey(), credential);
    return credential;
  }
}
