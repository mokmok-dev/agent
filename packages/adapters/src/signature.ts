import { AdapterError } from "./events.ts";
import { buffer, bytesEqual, toHex, utf8 } from "./bytes.ts";

const HASH_NAMES = { sha256: "SHA-256", sha1: "SHA-1" } as const;

export async function hmacHex(
  secret: string,
  message: string,
  hash: "sha256" | "sha1",
): Promise<string> {
  // WebCrypto refuses an empty HMAC key, so a missing secret fails here.
  if (secret === "") throw new AdapterError(401, "signing secret is empty");
  const key = await importHmacKey(secret, hash);
  return toHex(await crypto.subtle.sign("HMAC", key, buffer(utf8(message))));
}

export function constantTimeEqual(a: string, b: string): boolean {
  return bytesEqual(utf8(a), utf8(b));
}

export async function verifyHmac(
  secret: string,
  message: string,
  signature: string,
  hash: "sha256" | "sha1",
): Promise<void> {
  const expected = await hmacHex(secret, message, hash);
  if (!constantTimeEqual(expected, signature)) throw new AdapterError(401, "invalid signature");
}

export async function signSha256(secret: string, message: string): Promise<string> {
  return hmacHex(secret, message, "sha256");
}

/**
 * A secret as bytes. Providers hand them out as hex or base64, so a string
 * that is all hex pairs reads as hex and anything else reads as base64.
 */
export function decodeSecret(secret: string): Uint8Array {
  if (!/^(?:[0-9a-f]{2})+$/i.test(secret)) {
    try {
      return Uint8Array.from(atob(secret), (character) => character.charCodeAt(0));
    } catch {
      throw new AdapterError(401, "invalid secret encoding");
    }
  }
  return Uint8Array.from({ length: secret.length / 2 }, (_, index) =>
    Number.parseInt(secret.slice(index * 2, index * 2 + 2), 16),
  );
}

export async function verifyEd25519(
  publicKeyHex: string,
  signatureHex: string,
  message: string,
): Promise<void> {
  const algorithm = { name: "Ed25519" };
  const key = await importPublicKey(decodeSecret(publicKeyHex), algorithm);
  const signature = buffer(decodeSecret(signatureHex));
  const verified = await crypto.subtle.verify(algorithm, key, signature, buffer(utf8(message)));
  if (!verified) throw new AdapterError(401, "invalid signature");
}

async function importPublicKey(
  publicKey: Uint8Array,
  algorithm: { name: string },
): Promise<CryptoKey> {
  try {
    return await crypto.subtle.importKey("raw", buffer(publicKey), algorithm, false, ["verify"]);
  } catch {
    throw new AdapterError(401, "invalid public key");
  }
}

function importHmacKey(secret: string, hash: "sha256" | "sha1"): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    buffer(utf8(secret)),
    { name: "HMAC", hash: HASH_NAMES[hash] },
    false,
    ["sign"],
  );
}
