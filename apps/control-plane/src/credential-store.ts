import type { Credential, CredentialInfo, CredentialStore } from "@earendil-works/pi-ai";
import type { StorageHost } from "./storage.ts";

const key = (providerId: string): string => `credential:${providerId}`;

/**
 * The provider credentials a Durable Object holds, in its own storage.
 *
 * Pi defines the contract and this fills it, so a provider added by OAuth and a
 * provider added by an API key take the same path. The write is serialized
 * because a Durable Object already serializes its own calls, and pi runs an
 * OAuth refresh inside `modify`, so two requests cannot double-refresh a
 * rotated token.
 *
 * A credential is stored as it came, without redaction, because it has to be
 * usable. The object is what protects it; callers must never log it, and the
 * status route reports only the provider name and type.
 */
export class ObjectCredentialStore implements CredentialStore {
  readonly #storage: StorageHost;

  constructor(storage: StorageHost) {
    this.#storage = storage;
  }

  async read(providerId: string): Promise<Credential | undefined> {
    return this.#storage.get<Credential>(key(providerId));
  }

  async list(): Promise<readonly CredentialInfo[]> {
    const all = await this.#storage.list<Credential>({ prefix: "credential:" });
    return [...all.entries()].map(([name, credential]) => ({
      providerId: name.slice("credential:".length),
      type: credential.type,
    }));
  }

  /**
   * The only write path. `update` sees the current credential because a correct
   * refresh needs the token it replaces, and returning `undefined` leaves the
   * entry unchanged, which is how a failed refresh stays a no-op.
   */
  async modify(
    providerId: string,
    update: (current: Credential | undefined) => Promise<Credential | undefined>,
  ): Promise<Credential | undefined> {
    const current = await this.read(providerId);
    const next = await update(current);
    if (next === undefined) return current;
    await this.#storage.put(key(providerId), next);
    return next;
  }

  /** Remove a credential, which is a logout. */
  async delete(providerId: string): Promise<void> {
    await this.#storage.delete(key(providerId));
  }

  /**
   * Store a credential from a login or from an operator. This is `modify` with
   * a writer that always replaces, so every write goes through one path.
   */
  async put(providerId: string, credential: Credential): Promise<void> {
    await this.modify(providerId, async () => credential);
  }
}
