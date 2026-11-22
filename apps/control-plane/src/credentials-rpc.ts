import type { Credential, CredentialInfo, CredentialStore } from "@earendil-works/pi-ai";

/**
 * The credential object's RPC surface, as the Worker calls it. These are the
 * methods that do not take an options bag, because a Durable Object RPC cannot
 * serialize an `AbortSignal`.
 */
export interface ProviderRpc {
  read(providerId: string): Promise<Credential | undefined>;
  list(): Promise<readonly CredentialInfo[]>;
  write(providerId: string, credential: Credential): Promise<void>;
  delete(providerId: string): Promise<void>;
}

/**
 * Pi's `CredentialStore` over the deployment's provider object.
 *
 * Pi calls `read(providerId, { signal })` and `modify(providerId, update)`. A
 * Durable Object RPC refuses to serialize the `AbortSignal` in those options, so
 * a stub passed straight to pi fails with "AbortSignal serialization is not
 * enabled" before any credential is read. This adapter drops the options at the
 * boundary, which is the only fix available on this side of it: an in-flight
 * refresh is cancelled by the turn's own timeout rather than by the signal.
 *
 * `modify` runs here rather than on the object, because pi's callback cannot
 * cross an RPC either. The read-modify-write still converges, because a Durable
 * Object serializes its own calls and the write is the last step.
 */
export function credentialStoreOver(rpc: ProviderRpc): CredentialStore {
  return {
    read: (providerId: string) => rpc.read(providerId),
    list: () => rpc.list(),
    delete: (providerId: string) => rpc.delete(providerId),
    modify: async (
      providerId: string,
      update: (current: Credential | undefined) => Promise<Credential | undefined>,
    ) => {
      const current = await rpc.read(providerId);
      const next = await update(current);
      if (next === undefined) return current;
      await rpc.write(providerId, next);
      return next;
    },
  };
}
