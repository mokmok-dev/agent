import { beforeAll, describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  constantTimeEqual,
  decodeSecret,
  hmacHex,
  signSha256,
  verifyEd25519,
  verifyHmac,
} from "./signature.ts";
import { AdapterError } from "./events.ts";

const encoder = new TextEncoder();

async function rejection(promise: Promise<unknown>): Promise<AdapterError> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(AdapterError);
    return error as AdapterError;
  }
  throw new Error("expected an AdapterError");
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

  test("a prefix of the other is never equal", () => {
    fc.assert(
      fc.property(fc.string(), fc.string({ minLength: 1 }), (left, suffix) => {
        expect(constantTimeEqual(left, left + suffix)).toBe(false);
      }),
    );
  });
});

describe("hmacHex", () => {
  test("matches a known SHA-256 vector", async () => {
    // RFC 4231, test case 2.
    expect(await hmacHex("Jefe", "what do ya want for nothing?", "sha256")).toBe(
      "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
    );
  });

  test("matches a known SHA-1 vector", async () => {
    // RFC 2202, test case 2.
    expect(await hmacHex("Jefe", "what do ya want for nothing?", "sha1")).toBe(
      "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
    );
  });

  test("signSha256 is the SHA-256 form", async () => {
    expect(await signSha256("Jefe", "what do ya want for nothing?")).toBe(
      await hmacHex("Jefe", "what do ya want for nothing?", "sha256"),
    );
  });

  test("the digest is 64 lowercase hex characters", async () => {
    fc.assert(
      fc.asyncProperty(fc.string({ minLength: 1 }), fc.string(), async (secret, message) => {
        const digest = await hmacHex(secret, message, "sha256");
        expect(digest).toMatch(/^[0-9a-f]{64}$/);
      }),
    );
  });
});

describe("verifyHmac", () => {
  test("accepts the matching digest and rejects another", async () => {
    const secret = "shh";
    const message = '{"a":1}';
    const digest = await hmacHex(secret, message, "sha256");
    await verifyHmac(secret, message, digest, "sha256");
    expect(
      (await rejection(verifyHmac(secret, message, digest.toUpperCase(), "sha256"))).status,
    ).toBe(401);
    expect((await rejection(verifyHmac("other", message, digest, "sha256"))).status).toBe(401);
  });
});

describe("decodeSecret", () => {
  test("reads hex", () => {
    expect([...decodeSecret("ff00ab")]).toEqual([255, 0, 171]);
  });

  test("reads base64", () => {
    expect([...decodeSecret(Buffer.from("hello").toString("base64"))]).toEqual([
      104, 101, 108, 108, 111,
    ]);
  });

  test("a hex string round trips through base64", () => {
    const bytes = [0, 1, 127, 128, 255];
    const hex = bytes.map((byte) => byte.toString(16).padStart(2, "0")).join("");
    expect([...decodeSecret(hex)]).toEqual(bytes);
  });
});

describe("verifyEd25519", () => {
  let publicKeyHex: string;
  let privateKey: CryptoKey;

  beforeAll(async () => {
    const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
      "sign",
      "verify",
    ])) as unknown as CryptoKeyPair;
    privateKey = pair.privateKey;
    const raw = (await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer;
    publicKeyHex = [...new Uint8Array(raw)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
  });

  const sign = async (message: string): Promise<string> => {
    const signature = await crypto.subtle.sign(
      { name: "Ed25519" },
      privateKey,
      encoder.encode(message),
    );
    return [...new Uint8Array(signature)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
  };

  test("accepts a real signature", async () => {
    const message = '1700000000{"type":1}';
    await verifyEd25519(publicKeyHex, await sign(message), message);
  });

  test("rejects a signature over another message", async () => {
    const signature = await sign("one message");
    expect(
      (await rejection(verifyEd25519(publicKeyHex, signature, "another message"))).status,
    ).toBe(401);
  });

  test("rejects a tampered signature", async () => {
    const message = "payload";
    const signature = await sign(message);
    const flipped = (signature[0] === "0" ? "1" : "0") + signature.slice(1);
    expect((await rejection(verifyEd25519(publicKeyHex, flipped, message))).status).toBe(401);
  });

  test("rejects a malformed public key", async () => {
    expect((await rejection(verifyEd25519("zz", await sign("x"), "x"))).status).toBe(401);
  });

  test("every message signed by the key verifies and no other does", async () => {
    await fc.assert(
      fc.asyncProperty(fc.string(), fc.string({ minLength: 1 }), async (message, other) => {
        const signature = await sign(message);
        await verifyEd25519(publicKeyHex, signature, message);
        if (message === other) return;
        const error = await rejection(verifyEd25519(publicKeyHex, signature, other));
        expect(error.status).toBe(401);
      }),
    );
  });
});

describe("an empty secret", () => {
  test("is refused rather than producing a digest", async () => {
    expect((await rejection(hmacHex("", "message", "sha256"))).status).toBe(401);
  });
});
