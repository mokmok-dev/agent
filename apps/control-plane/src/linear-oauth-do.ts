import {
  exchangeCode,
  needsRefresh,
  postForm,
  readCallback,
  refreshTokens,
  revokeToken,
  type LinearTokens,
  type LinearOAuthConfig,
  type TokenRequest,
} from "@agent/adapters";
import type { StorageHost } from "./storage.ts";

/** The stored authorization for one Linear workspace. */
export type LinearCredential = LinearTokens & {
  /** The workspace the token belongs to, once a request reports it. */
  organizationId: string | null;
};

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

  constructor(storage: StorageHost, config: LinearOAuthConfig, request: TokenRequest = postForm) {
    this.#storage = storage;
    this.#config = config;
    this.#request = request;
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
   * refused even when the code exchange would otherwise succeed twice.
   */
  async complete(params: Record<string, string>, now: number): Promise<LinearCredential> {
    const expected = expectedState(await this.#storage.get<unknown>(stateKey()));
    const callback = readCallback(params, expected);
    await this.#storage.delete(stateKey());
    const tokens = await exchangeCode(this.#config, callback.code, now, this.#request);
    return this.#store({ ...tokens, organizationId: null });
  }

  /** The stored credential, or `undefined` before the first login. */
  credential(): Promise<LinearCredential | undefined> {
    return this.#storage.get<LinearCredential>(credentialKey());
  }

  /**
   * A usable access token. A pair inside the refresh skew is renewed first, and
   * the renewed pair replaces the stored one.
   */
  async accessToken(now: number): Promise<string | undefined> {
    const credential = await this.credential();
    if (credential === undefined) return undefined;
    if (!needsRefresh(credential, now)) return credential.accessToken;
    return (await this.#renew(credential, now)).accessToken;
  }

  /** Refresh now, whatever the expiry says. A 401 on a request calls this. */
  async refresh(now: number): Promise<boolean> {
    const credential = await this.credential();
    if (credential === undefined) return false;
    await this.#renew(credential, now);
    return true;
  }

  /** Give up the authorization. The stored pair is removed whether or not Linear answers. */
  async disconnect(): Promise<void> {
    const credential = await this.credential();
    if (credential !== undefined) await revokeToken(credential.accessToken, this.#request);
    await this.#storage.delete(credentialKey());
  }

  async #renew(credential: LinearCredential, now: number): Promise<LinearCredential> {
    const renewed = await refreshTokens(this.#config, credential.refreshToken, now, this.#request);
    return this.#store({ ...renewed, organizationId: credential.organizationId });
  }

  #store(credential: LinearCredential): Promise<LinearCredential> {
    return this.#storage.put(credentialKey(), credential).then(() => credential);
  }
}
