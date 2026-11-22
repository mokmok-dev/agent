import { beforeAll, describe, expect, test } from "bun:test";
import {
  constantTimeEqual,
  decodeSecret,
  hmacHex,
  signSha256,
  verifyEd25519,
  verifyHmac,
  type Signature,
  type SignatureRefused,
  type Verified,
} from "./signature.ts";

const encoder = new TextEncoder();

/** The refusal a signature check returned, or a failure naming the value. */
function refusal(answer: Verified): SignatureRefused {
  if (answer.kind === "verified") throw new Error("expected a refusal");
  return answer;
}

/** The bytes a secret decoded to, or a failure naming the refusal. */
function bytes(decoded: ReturnType<typeof decodeSecret>): Uint8Array {
  if (decoded.kind !== "bytes") throw new Error("expected bytes");
  return decoded.value;
}

/** The hex a signature produced, or a failure naming the refusal. */
function signed(result: Signature): string {
  if (result.kind !== "signed") throw new Error("expected a signature");
  return result.hex;
}

describe("constantTimeEqual", () => {
  test("is true only for equal strings", () => {
    expect(constantTimeEqual("abc", "abc")).toBe(true);
    expect(constantTimeEqual("", "")).toBe(true);
    expect(constantTimeEqual("abc", "abd")).toBe(false);
    expect(constantTimeEqual("abc", "ab")).toBe(false);
    expect(constantTimeEqual("ab", "abc")).toBe(false);
    expect(constantTimeEqual("", "a")).toBe(false);
  });
});

describe("hmacHex", () => {
  test("produces the documented SHA-256 digest", async () => {
    expect(
      signed(await hmacHex("key", "The quick brown fox jumps over the lazy dog", "sha256")),
    ).toBe("f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8");
  });

  test("produces the documented SHA-1 digest", async () => {
    expect(
      signed(await hmacHex("key", "The quick brown fox jumps over the lazy dog", "sha1")),
    ).toBe("de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9");
  });

  test("an empty secret is unsigned, because WebCrypto will not key it", async () => {
    expect(await hmacHex("", "m", "sha256")).toEqual({
      kind: "unsigned",
      message: "signing secret is empty",
    });
  });

  test("signSha256 is the same function under a narrower name", async () => {
    expect(signed(await signSha256("k", "m"))).toBe(signed(await hmacHex("k", "m", "sha256")));
  });
});

describe("verifyHmac", () => {
  test("accepts the matching digest and reports a mismatch for another", async () => {
    const secret = "shh";
    const message = '{"a":1}';
    const digest = signed(await hmacHex(secret, message, "sha256"));
    expect(await verifyHmac(secret, message, digest, "sha256")).toEqual({ kind: "verified" });
    expect(await verifyHmac(secret, message, digest.toUpperCase(), "sha256")).toEqual({
      kind: "mismatch",
    });
    expect(await verifyHmac("other", message, digest, "sha256")).toEqual({ kind: "mismatch" });
  });

  test("an empty secret passes the refusal through", async () => {
    expect(await verifyHmac("", "m", "sig", "sha256")).toEqual({
      kind: "unsigned",
      message: "signing secret is empty",
    });
  });
});

describe("decodeSecret", () => {
  test("reads hex", () => {
    expect([...bytes(decodeSecret("ff00ab"))]).toEqual([255, 0, 171]);
  });

  test("reads base64", () => {
    expect([...bytes(decodeSecret(Buffer.from("hello").toString("base64")))]).toEqual([
      ...encoder.encode("hello"),
    ]);
  });

  test("an odd-length hex-looking string reads as base64", () => {
    expect([...bytes(decodeSecret("abc"))]).toEqual([
      ...Uint8Array.from(atob("abc"), (c) => c.charCodeAt(0)),
    ]);
  });

  test("a string that is neither hex nor base64 is undecodable", () => {
    expect(decodeSecret("not!base64")).toEqual({ kind: "undecodable" });
  });
});

describe("verifyEd25519", () => {
  let publicKeyHex = "";

  beforeAll(async () => {
    const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
      "sign",
      "verify",
    ])) as unknown as CryptoKeyPair;
    const raw = (await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer;
    publicKeyHex = [...new Uint8Array(raw)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
  });

  test("accepts a signature made by the matching key", async () => {
    const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
      "sign",
      "verify",
    ])) as unknown as CryptoKeyPair;
    const raw = (await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer;
    const hex = [...new Uint8Array(raw)].map((b) => b.toString(16).padStart(2, "0")).join("");
    const signature = new Uint8Array(
      await crypto.subtle.sign({ name: "Ed25519" }, pair.privateKey, encoder.encode("message")),
    );
    const signatureHex = [...signature].map((b) => b.toString(16).padStart(2, "0")).join("");
    expect(await verifyEd25519(hex, signatureHex, "message")).toEqual({ kind: "verified" });
  });

  test("a signature over another message is not verified", async () => {
    expect(refusal(await verifyEd25519(publicKeyHex, "00".repeat(64), "message")).kind).not.toBe(
      "verified",
    );
  });

  test("a key that is not Ed25519 is unusable", async () => {
    expect(await verifyEd25519("aabb", "aa", "m")).toEqual({
      kind: "unusable",
      message: "invalid public key",
    });
  });

  test("a signature that is not hex or base64 is unusable", async () => {
    expect(await verifyEd25519(publicKeyHex, "not!base64", "m")).toEqual({
      kind: "unusable",
      message: "invalid signature encoding",
    });
  });
});

describe("properties", () => {
  test("a digest is always 64 lowercase hex characters", async () => {
    for (const secret of ["a", "shh", "x".repeat(40)]) {
      for (const message of ["", "m", '{"a":1}']) {
        const digest = await hmacHex(secret, message, "sha256");
        if (digest.kind !== "signed") throw new Error("expected a signature");
        expect(digest.hex).toMatch(/^[0-9a-f]{64}$/);
      }
    }
  });
});
