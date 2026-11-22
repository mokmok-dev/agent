import { buffer, bytesEqual, toHex, utf8 } from "./bytes.ts";

const HASH_NAMES = { sha256: "SHA-256", sha1: "SHA-1" } as const;

/**
 * Why a signature could not be established. `absent` is a missing or empty
 * header, which an operator fixes in the provider; `mismatch` is a request that
 * is not authentic; `unusable` is a key or encoding the platform rejected;
 * `unsigned` is a deployment with no secret.
 */
export type SignatureRefused =
  | { kind: "absent"; header: string }
  | { kind: "unsigned"; message: string }
  | { kind: "mismatch" }
  | { kind: "unusable"; message: string };

export type Signature = { kind: "signed"; hex: string } | SignatureRefused;
export type Verified = { kind: "verified" } | SignatureRefused;

/**
 * A header a provider signs with. A missing header and an empty one are the
 * same refusal to a verifier: the request carries nothing to check.
 */
export function headerOf(
  request: { headers: Record<string, string | undefined> },
  name: string,
): Verified | { kind: "present"; value: string } {
  const value = request.headers[name];
  return value === undefined || value === ""
    ? { kind: "absent", header: name }
    : { kind: "present", value };
}

/**
 * An HMAC as hex. WebCrypto will not key an empty secret, so that is reported
 * as an unsigned deployment rather than a failed request.
 */
export async function hmacHex(
  secret: string,
  message: string,
  hash: "sha256" | "sha1",
): Promise<Signature> {
  // An empty secret is refused here because WebCrypto throws on it; every other
  // secret imports, so the import cannot fail on its own.
  if (secret === "") return { kind: "unsigned", message: "signing secret is empty" };
  return signWith(await importHmacKey(secret, hash), message);
}

type Keyed = { kind: "keyed"; value: CryptoKey };

async function signWith(key: CryptoKey, message: string): Promise<Signature> {
  // A keyed HMAC and a string both satisfy the platform, so this cannot throw.
  const hex = toHex(await crypto.subtle.sign("HMAC", key, buffer(utf8(message))));
  return { kind: "signed", hex };
}

export function constantTimeEqual(a: string, b: string): boolean {
  return bytesEqual(utf8(a), utf8(b));
}

/** Whether a signature matches. A mismatch and a computation failure both mean "not authentic". */
export async function verifyHmac(
  secret: string,
  message: string,
  signature: string,
  hash: "sha256" | "sha1",
): Promise<Verified> {
  const expected = await hmacHex(secret, message, hash);
  if (expected.kind !== "signed") return expected;
  return constantTimeEqual(expected.hex, signature) ? { kind: "verified" } : { kind: "mismatch" };
}

export async function signSha256(secret: string, message: string): Promise<Signature> {
  return hmacHex(secret, message, "sha256");
}

export type SecretBytes = { kind: "bytes"; value: Uint8Array } | { kind: "undecodable" };

/**
 * A secret as bytes. Providers hand them out as hex or base64, so a string that
 * is all hex pairs reads as hex and anything else reads as base64.
 */
export function decodeSecret(secret: string): SecretBytes {
  if (/^(?:[0-9a-f]{2})+$/i.test(secret)) {
    return {
      kind: "bytes",
      value: Uint8Array.from({ length: secret.length / 2 }, (_, index) =>
        Number.parseInt(secret.slice(index * 2, index * 2 + 2), 16),
      ),
    };
  }
  try {
    return {
      kind: "bytes",
      value: Uint8Array.from(atob(secret), (character) => character.charCodeAt(0)),
    };
  } catch {
    return { kind: "undecodable" };
  }
}

/** Whether an Ed25519 signature matches. A malformed key or signature is `unusable`. */
export async function verifyEd25519(
  publicKeyHex: string,
  signatureHex: string,
  message: string,
): Promise<Verified> {
  const keyBytes = decodeSecret(publicKeyHex);
  if (keyBytes.kind !== "bytes")
    return { kind: "unusable", message: "invalid public key encoding" };
  const signatureBytes = decodeSecret(signatureHex);
  if (signatureBytes.kind !== "bytes") {
    return { kind: "unusable", message: "invalid signature encoding" };
  }
  const key = await importPublicKey(keyBytes.value);
  if (key.kind !== "keyed") return key;
  return verifyWith(key.value, signatureBytes.value, message);
}

async function verifyWith(
  key: CryptoKey,
  signature: Uint8Array,
  message: string,
): Promise<Verified> {
  // A keyed Ed25519 key and fixed-size buffers satisfy the platform, so this
  // cannot throw; a bad signature is the `mismatch` below.
  const verified = await crypto.subtle.verify(
    { name: "Ed25519" },
    key,
    buffer(signature),
    buffer(utf8(message)),
  );
  return verified ? { kind: "verified" } : { kind: "mismatch" };
}

async function importPublicKey(publicKey: Uint8Array): Promise<Keyed | SignatureRefused> {
  try {
    const value = await crypto.subtle.importKey(
      "raw",
      buffer(publicKey),
      { name: "Ed25519" },
      false,
      ["verify"],
    );
    return { kind: "keyed", value };
  } catch {
    return { kind: "unusable", message: "invalid public key" };
  }
}

/**
 * The HMAC key. The caller has already refused an empty secret, which is the
 * only input WebCrypto rejects, so this cannot fail.
 */
function importHmacKey(secret: string, hash: "sha256" | "sha1"): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    buffer(utf8(secret)),
    { name: "HMAC", hash: HASH_NAMES[hash] },
    false,
    ["sign"],
  );
}
