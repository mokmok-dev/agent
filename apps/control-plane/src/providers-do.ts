import type { Credential, CredentialInfo } from "@earendil-works/pi-ai";
import type { StorageHost } from "./storage.ts";
import { ObjectCredentialStore } from "./credential-store.ts";

/**
 * The provider credentials for the whole deployment.
 *
 * There is one object, named `providers`, because a credential an operator
 * stores applies to every session: putting it in a session's own storage would
 * mean logging in once per conversation. The storage mechanics are the same as
 * any other object, so this composes the store rather than repeating it.
 */
export class ProviderCredentialObject {
  readonly #store: ObjectCredentialStore;

  constructor(storage: StorageHost) {
    this.#store = new ObjectCredentialStore(storage);
  }

  read(providerId: string): Promise<Credential | undefined> {
    return this.#store.read(providerId);
  }

  list(): Promise<readonly CredentialInfo[]> {
    return this.#store.list();
  }

  /**
   * The only write path across the RPC boundary. A callback cannot cross a
   * Durable Object RPC, so the caller sends the credential it wants written and
   * the object serializes the write with everyone else's.
   */
  async write(providerId: string, credential: Credential): Promise<void> {
    await this.#store.put(providerId, credential);
  }

  delete(providerId: string): Promise<void> {
    return this.#store.delete(providerId);
  }
}
