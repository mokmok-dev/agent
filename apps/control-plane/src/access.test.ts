import { describe, expect, test } from "bun:test";
import { accessRefusal } from "./worker.ts";
import {
  ACCESS_JWT_HEADER,
  accessConfigOf,
  base64UrlToBytes,
  verifyAccess,
  type AccessConfig,
} from "./access.ts";

/**
 * The Access gate, driven with a real RSA key.
 *
 * A fake signature would prove nothing: the whole point is that a token the
 * team did not sign is refused. These tests mint tokens with a key they hold
 * and present the matching public key to the verifier, so a broken signature
 * check fails here.
 */

const config: AccessConfig = {
  teamDomain: "mokmok.cloudflareaccess.com",
  audience: "b412b13d9aa480076580a7a4ba2619a9f92aeb777268c36b030b470cd0dc6ca2",
};

const encoder = new TextEncoder();

const base64Url = (bytes: Uint8Array): string =>
  btoa(String.fromCharCode(...bytes))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");

const pair = (await crypto.subtle.generateKey(
  {
    name: "RSASSA-PKCS1-v1_5",
    modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]),
    hash: "SHA-256",
  },
  true,
  ["sign", "verify"],
)) as CryptoKeyPair;

const exported = (await crypto.subtle.exportKey("jwk", pair.publicKey)) as JsonWebKey;
const kid = "test-key-1";

const jwks = { keys: [{ ...exported, kid, alg: "RS256", use: "sig" }] };

const signerOf =
  (key: CryptoKey) =>
  async (signed: string): Promise<string> => {
    const signature = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, encoder.encode(signed));
    return base64Url(new Uint8Array(signature));
  };

const claimsFor = (overrides: Record<string, unknown> = {}) => ({
  iss: `https://${config.teamDomain}`,
  aud: [config.audience],
  exp: Math.floor(Date.now() / 1000) + 3600,
  iat: Math.floor(Date.now() / 1000),
  common_name: "gateway-1",
  ...overrides,
});

/** A token from `claims`, signed with the test key unless another is given. */
async function token(
  claims: Record<string, unknown>,
  options: { key?: CryptoKey; header?: Record<string, unknown> } = {},
): Promise<string> {
  const header = { alg: "RS256", kid, typ: "JWT", ...options.header };
  const head = base64Url(encoder.encode(JSON.stringify(header)));
  const body = base64Url(encoder.encode(JSON.stringify(claims)));
  const signed = `${head}.${body}`;
  const signature = await signerOf(options.key ?? pair.privateKey)(signed);
  return `${signed}.${signature}`;
}

const requestWith = (jwt?: string): Request => {
  const headers: Record<string, string> = {};
  if (jwt !== undefined) headers[ACCESS_JWT_HEADER] = jwt;
  return new Request("https://x/gateway", { headers });
};

const keysFrom =
  (body: unknown, status = 200) =>
  async () => ({ status, body });
const neverFetched = async () => {
  throw new Error("the verifier fetched keys it did not need");
};

describe("the Access settings", () => {
  test("both a team domain and an audience are needed, because either alone checks nothing", () => {
    expect(accessConfigOf({})).toBeUndefined();
    expect(accessConfigOf({ ACCESS_TEAM_DOMAIN: "mokmok.cloudflareaccess.com" })).toBeUndefined();
    expect(accessConfigOf({ ACCESS_AUD: "aud" })).toBeUndefined();
    expect(
      accessConfigOf({ ACCESS_TEAM_DOMAIN: "mokmok.cloudflareaccess.com", ACCESS_AUD: "aud" }),
    ).toEqual({ teamDomain: "mokmok.cloudflareaccess.com", audience: "aud" });
  });

  test("a stated setting is trimmed, so a pasted secret still works", () => {
    expect(
      accessConfigOf({ ACCESS_TEAM_DOMAIN: "  team.example.com  ", ACCESS_AUD: " aud " }),
    ).toEqual({ teamDomain: "team.example.com", audience: "aud" });
  });

  test("a blank setting counts as unstated", () => {
    expect(accessConfigOf({ ACCESS_TEAM_DOMAIN: "   ", ACCESS_AUD: "aud" })).toBeUndefined();
  });
});

describe("a request with no token", () => {
  test("is refused before any key is fetched", async () => {
    expect(await verifyAccess(requestWith(), config, Date.now(), neverFetched)).toEqual({
      kind: "refused",
      refusal: { kind: "no-token" },
    });
  });

  test("an empty header is the same as no header", async () => {
    expect(await verifyAccess(requestWith("  "), config, Date.now(), neverFetched)).toEqual({
      kind: "refused",
      refusal: { kind: "no-token" },
    });
  });
});

describe("a token that is not shaped like a JWT", () => {
  test("is refused by name rather than read as claims", async () => {
    for (const bad of ["not-a-jwt", "a.b", "a.b.c.d"]) {
      const outcome = await verifyAccess(requestWith(bad), config, Date.now(), neverFetched);
      expect(outcome).toEqual({
        kind: "refused",
        refusal: { kind: "malformed", reason: "expected three parts" },
      });
    }
  });

  test("a segment that is not JSON is refused", async () => {
    const bogus = `${base64Url(encoder.encode("nope"))}.${base64Url(encoder.encode("{}"))}.x`;
    expect(await verifyAccess(requestWith(bogus), config, Date.now(), neverFetched)).toEqual({
      kind: "refused",
      refusal: { kind: "malformed", reason: "expected JSON header and payload" },
    });
  });

  test("a header with no key id is refused, because no key can be chosen", async () => {
    const head = base64Url(encoder.encode(JSON.stringify({ alg: "RS256" })));
    const body = base64Url(encoder.encode(JSON.stringify(claimsFor())));
    expect(
      await verifyAccess(requestWith(`${head}.${body}.sig`), config, Date.now(), neverFetched),
    ).toEqual({
      kind: "refused",
      refusal: { kind: "malformed", reason: "no key id in the header" },
    });
  });

  test("an algorithm other than RS256 is refused, so a token cannot pick a weaker one", async () => {
    const jwt = await token(claimsFor(), { header: { alg: "none" } });
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), neverFetched)).toEqual({
      kind: "refused",
      refusal: { kind: "malformed", reason: "unsupported alg none" },
    });
  });
});

describe("the key the token names", () => {
  test("a key id the team's keys do not hold is refused by that id", async () => {
    const jwt = await token(claimsFor(), { header: { kid: "unknown" } });
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-key", kid: "unknown" },
    });
  });

  test("keys that cannot be fetched are a refusal, not a crash", async () => {
    const jwt = await token(claimsFor());
    expect(
      await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(undefined, 503)),
    ).toEqual({ kind: "refused", refusal: { kind: "keys-unavailable", status: 503 } });
  });

  test("a certs document with no keys reads as an unknown key", async () => {
    const jwt = await token(claimsFor());
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom({}))).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-key", kid },
    });
  });

  test("a certs entry that is not an object is passed over", async () => {
    const jwt = await token(claimsFor());
    expect(
      await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom({ keys: [null, 7] })),
    ).toEqual({ kind: "refused", refusal: { kind: "unknown-key", kid } });
  });
});

describe("a token someone else signed", () => {
  test("is refused, however well formed it is", async () => {
    const otherPair = (await crypto.subtle.generateKey(
      {
        name: "RSASSA-PKCS1-v1_5",
        modulusLength: 2048,
        publicExponent: new Uint8Array([1, 0, 1]),
        hash: "SHA-256",
      },
      true,
      ["sign", "verify"],
    )) as CryptoKeyPair;
    // Same claims, same kid, different private key: exactly a forged token.
    const jwt = await token(claimsFor(), { key: otherPair.privateKey });
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "bad-signature" },
    });
  });

  test("a key without a modulus cannot verify, so it is refused rather than crashing", async () => {
    const jwt = await token(claimsFor());
    const noModulus = { keys: [{ kid, kty: "RSA" }] };
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(noModulus))).toEqual({
      kind: "refused",
      refusal: { kind: "bad-signature" },
    });
  });
});

describe("the claims a signed token must carry", () => {
  test("a token for another team is refused, naming the issuer it stated", async () => {
    const jwt = await token(claimsFor({ iss: "https://other.cloudflareaccess.com" }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-issuer", issuer: "https://other.cloudflareaccess.com" },
    });
  });

  test("a token minted for another application is refused", async () => {
    const jwt = await token(claimsFor({ aud: ["another-app"] }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-audience" },
    });
  });

  test("an audience stated as one string is accepted, because Access uses both forms", async () => {
    const jwt = await token(claimsFor({ aud: config.audience }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });

  test("an audience that is neither a string nor a list is refused", async () => {
    const jwt = await token(claimsFor({ aud: 7 }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-audience" },
    });
  });

  test("an expired token is refused, so a leaked one stops working", async () => {
    const jwt = await token(claimsFor({ exp: Math.floor(Date.now() / 1000) - 1 }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "expired" },
    });
  });

  test("a token with no expiry is refused rather than trusted forever", async () => {
    const jwt = await token(claimsFor({ exp: undefined }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "expired" },
    });
  });

  test("a token used before its not-before time is refused", async () => {
    const jwt = await token(claimsFor({ nbf: Math.floor(Date.now() / 1000) + 600 }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "expired" },
    });
  });
});

describe("a token Access issued for this application", () => {
  test("is verified, and names the service token it came from", async () => {
    const jwt = await token(claimsFor());
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    // The name an operator gave the service token is what a log line should
    // say, so which machine connected is answerable.
    expect(outcome.value.subject).toBe("gateway-1");
  });

  test("a person's token names them when there is no service-token name", async () => {
    const jwt = await token(claimsFor({ common_name: undefined, email: "someone@example.com" }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    expect(outcome.value.subject).toBe("someone@example.com");
  });

  test("a token with neither name falls back to the subject id", async () => {
    const jwt = await token(claimsFor({ common_name: undefined, sub: "user-uuid" }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    expect(outcome.value.subject).toBe("user-uuid");
  });

  test("the claims come back, so a caller can read what it needs", async () => {
    const jwt = await token(claimsFor());
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    expect(outcome.value.claims.iss).toBe(`https://${config.teamDomain}`);
  });
});

describe("the base64url decoder", () => {
  test("decodes the alphabet a JWT uses", () => {
    // Both characters that differ from base64, and a length that needs padding.
    expect(new TextDecoder().decode(base64UrlToBytes("aGVsbG8"))).toBe("hello");
    expect(new TextDecoder().decode(base64UrlToBytes("aGVsbG8_"))).toContain("hello");
  });
});

describe("the certs document is read from the team domain", () => {
  test("the URL is the team domain plus the access certs path", async () => {
    // A wrong path silently reads no keys and refuses every gateway, which is
    // indistinguishable from a forged token from the outside.
    const asked: string[] = [];
    const jwt = await token(claimsFor());
    await verifyAccess(requestWith(jwt), config, Date.now(), async (url) => {
      asked.push(url);
      return { status: 200, body: jwks };
    });
    expect(asked).toEqual([`https://${config.teamDomain}/cdn-cgi/access/certs`]);
  });
});

describe("a token with surrounding whitespace", () => {
  test("surrounding whitespace is ignored, because a header value arrives as sent", async () => {
    const jwt = await token(claimsFor());
    const outcome = await verifyAccess(
      requestWith(`  ${jwt}  `),
      config,
      Date.now(),
      keysFrom(jwks),
    );
    expect(outcome.kind).toBe("verified");
  });
});

describe("a payload that is not JSON", () => {
  test("is refused as malformed, naming the payload", async () => {
    const head = base64Url(encoder.encode(JSON.stringify({ alg: "RS256", kid })));
    const body = base64Url(encoder.encode("not json"));
    expect(
      await verifyAccess(requestWith(`${head}.${body}.sig`), config, Date.now(), neverFetched),
    ).toEqual({
      kind: "refused",
      refusal: { kind: "malformed", reason: "expected JSON header and payload" },
    });
  });
});

describe("an empty key id", () => {
  test("is refused, because no key can be chosen for it", async () => {
    const head = base64Url(encoder.encode(JSON.stringify({ alg: "RS256", kid: "" })));
    const body = base64Url(encoder.encode(JSON.stringify(claimsFor())));
    expect(
      await verifyAccess(requestWith(`${head}.${body}.sig`), config, Date.now(), neverFetched),
    ).toEqual({
      kind: "refused",
      refusal: { kind: "malformed", reason: "no key id in the header" },
    });
  });
});

describe("the issuer claim", () => {
  test("a missing issuer is refused, rather than read as the empty string matching", async () => {
    const jwt = await token(claimsFor({ iss: undefined }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-issuer", issuer: "" },
    });
  });

  test("an issuer that is not a string is refused", async () => {
    const jwt = await token(claimsFor({ iss: 7 }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-issuer", issuer: "" },
    });
  });
});

describe("an expiry exactly now", () => {
  test("is expired, because the token stops being valid at its exp", async () => {
    const at = Math.floor(Date.now() / 1000);
    const jwt = await token(claimsFor({ exp: at }));
    expect(await verifyAccess(requestWith(jwt), config, at * 1000, keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "expired" },
    });
  });

  test("one millisecond before expiry it is still valid", async () => {
    const at = Math.floor(Date.now() / 1000);
    const jwt = await token(claimsFor({ exp: at }));
    const outcome = await verifyAccess(requestWith(jwt), config, at * 1000 - 1, keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });
});

describe("the not-before claim", () => {
  test("is ignored when a token does not carry it", async () => {
    const jwt = await token(claimsFor({ nbf: undefined }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });

  test("a token exactly at its not-before time is valid", async () => {
    const at = Math.floor(Date.now() / 1000);
    const jwt = await token(claimsFor({ nbf: at }));
    const outcome = await verifyAccess(requestWith(jwt), config, at * 1000, keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });

  test("one second before is refused", async () => {
    const at = Math.floor(Date.now() / 1000);
    const jwt = await token(claimsFor({ nbf: at }));
    expect(await verifyAccess(requestWith(jwt), config, at * 1000 - 1000, keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "expired" },
    });
  });

  test("a not-before that is not a number is ignored, because it states no time", async () => {
    const jwt = await token(claimsFor({ nbf: "soon" }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });
});

describe("the subject a token names", () => {
  test("an empty service-token name falls through to the email", async () => {
    const jwt = await token(claimsFor({ common_name: "", email: "a@b.c" }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    expect(outcome.value.subject).toBe("a@b.c");
  });

  test("an empty email falls through to the subject id", async () => {
    const jwt = await token(claimsFor({ common_name: undefined, email: "", sub: "uuid" }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    expect(outcome.value.subject).toBe("uuid");
  });

  test("a subject that is not a string names nothing, rather than printing it", async () => {
    const jwt = await token(claimsFor({ common_name: undefined, email: undefined, sub: 7 }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    if (outcome.kind !== "verified") throw new Error("expected a verified token");
    expect(outcome.value.subject).toBe("");
  });
});

describe("an audience list", () => {
  test("that does not contain this application is refused", async () => {
    const jwt = await token(claimsFor({ aud: ["a", "b"] }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-audience" },
    });
  });

  test("that contains it among others is accepted, which is how Access states several", async () => {
    const jwt = await token(claimsFor({ aud: ["a", config.audience] }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });
});

describe("a certs entry that is not an object", () => {
  test("is passed over even when its key id matches", async () => {
    const jwt = await token(claimsFor());
    expect(
      await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom({ keys: ["str"] })),
    ).toEqual({ kind: "refused", refusal: { kind: "unknown-key", kid } });
  });

  test("a certs body that is not an object has no keys", async () => {
    const jwt = await token(claimsFor());
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom("nope"))).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-key", kid },
    });
  });
});

describe("a key that cannot be imported", () => {
  test("is a bad signature, not a crash", async () => {
    const jwt = await token(claimsFor());
    // A well-shaped RSA key with an exponent that cannot form a key.
    const broken = { keys: [{ kid, kty: "RSA", n: "AQAB", e: "AQAB" }] };
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(broken));
    expect(outcome.kind).toBe("refused");
  });
});

describe("the base64url decoder's edge cases", () => {
  test("a length that is already a multiple of four needs no padding", () => {
    expect(new TextDecoder().decode(base64UrlToBytes("YWJj"))).toBe("abc");
  });

  test("each padding length decodes", () => {
    // Lengths 2 and 3 mod 4 exercise both padding amounts.
    expect(new TextDecoder().decode(base64UrlToBytes("YQ"))).toBe("a");
    expect(new TextDecoder().decode(base64UrlToBytes("YWI"))).toBe("ab");
  });

  test("the whole input is decoded, including the last byte", () => {
    // A loop that stops one byte early loses the last character.
    expect(new TextDecoder().decode(base64UrlToBytes("YWJjZA"))).toBe("abcd");
  });
});

describe("the certs fetch itself", () => {
  const realFetch = globalThis.fetch;

  test("a response that is not ok is reported with its status", async () => {
    globalThis.fetch = (async () =>
      new Response("nope", { status: 500 })) as unknown as typeof fetch;
    try {
      const jwt = await token(claimsFor());
      expect(await verifyAccess(requestWith(jwt), config, Date.now())).toEqual({
        kind: "refused",
        refusal: { kind: "keys-unavailable", status: 500 },
      });
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test("an unreachable team domain is a 503 refusal, not a thrown error", async () => {
    globalThis.fetch = (async () => {
      throw new Error("offline");
    }) as unknown as typeof fetch;
    try {
      const jwt = await token(claimsFor());
      expect(await verifyAccess(requestWith(jwt), config, Date.now())).toEqual({
        kind: "refused",
        refusal: { kind: "keys-unavailable", status: 503 },
      });
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  test("a live response is read as JSON and used to verify", async () => {
    globalThis.fetch = (async () => Response.json(jwks)) as unknown as typeof fetch;
    try {
      const jwt = await token(claimsFor());
      const outcome = await verifyAccess(requestWith(jwt), config, Date.now());
      expect(outcome.kind).toBe("verified");
    } finally {
      globalThis.fetch = realFetch;
    }
  });
});

describe("the answer a refused gateway receives", () => {
  const answer = async (refusal: Parameters<typeof accessRefusal>[0]) => {
    const response = accessRefusal(refusal);
    return { status: response.status, body: (await response.json()) as Record<string, unknown> };
  };

  test("every refusal is a 401, because the caller can present a credential", async () => {
    const refusals = [
      { kind: "no-token" as const },
      { kind: "bad-signature" as const },
      { kind: "expired" as const },
      { kind: "wrong-audience" as const },
      { kind: "unknown-key" as const, kid: "k" },
      { kind: "wrong-issuer" as const, issuer: "https://other" },
      { kind: "malformed" as const, reason: "r" },
      { kind: "keys-unavailable" as const, status: 503 },
    ];
    for (const refusal of refusals) {
      expect((await answer(refusal)).status).toBe(401);
    }
  });

  test("the reason is named, so an operator does not have to guess", async () => {
    expect(await answer({ kind: "no-token" })).toEqual({
      status: 401,
      body: { error: "this route needs a Cloudflare Access token", refusal: "no-token" },
    });
    expect(await answer({ kind: "expired" })).toEqual({
      status: 401,
      body: { error: "the Access token has expired", refusal: "expired" },
    });
    expect(await answer({ kind: "wrong-audience" })).toEqual({
      status: 401,
      body: { error: "the Access token is for another application", refusal: "wrong-audience" },
    });
    expect(await answer({ kind: "bad-signature" })).toEqual({
      status: 401,
      body: { error: "the Access token signature does not verify", refusal: "bad-signature" },
    });
  });

  test("a refusal that carries detail repeats it in the message", async () => {
    // The key id and the issuer are what an operator checks against the team's
    // settings, so they must not be dropped.
    expect((await answer({ kind: "unknown-key", kid: "key-9" })).body.error).toContain("key-9");
    expect((await answer({ kind: "wrong-issuer", issuer: "https://other" })).body.error).toContain(
      "https://other",
    );
    expect((await answer({ kind: "malformed", reason: "three parts" })).body.error).toContain(
      "three parts",
    );
    expect((await answer({ kind: "keys-unavailable", status: 502 })).body.error).toContain("502");
  });
});

describe("a token that differs only in whitespace", () => {
  test("a token with no whitespace still verifies, so trimming is not what admits it", async () => {
    const jwt = await token(claimsFor());
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });

  test("whitespace inside a segment breaks the signature, so trimming only the ends matters", async () => {
    // Proves the trim is at the edges: an interior space is not repaired by it.
    const jwt = await token(claimsFor());
    const broken = `${jwt.slice(0, 10)} ${jwt.slice(10)}`;
    const outcome = await verifyAccess(requestWith(broken), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("refused");
  });
});

describe("the audience stated as one string", () => {
  test("is compared to the expected tag, so a different one is refused", async () => {
    // The string form must go through the same comparison as the list form; an
    // always-true answer here would accept a token for any application.
    const jwt = await token(claimsFor({ aud: "another-app" }));
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks))).toEqual({
      kind: "refused",
      refusal: { kind: "wrong-audience" },
    });
  });
});

describe("a key whose exponent is missing", () => {
  test("cannot verify, so the token is refused rather than crashing", async () => {
    const jwt = await token(claimsFor());
    const noExponent = { keys: [{ kid, kty: "RSA", n: exported.n }] };
    expect(await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(noExponent))).toEqual({
      kind: "refused",
      refusal: { kind: "bad-signature" },
    });
  });
});

describe("a certs entry that matches by key id but is not an object", () => {
  test("is not chosen, because its key id cannot be read from a non-object", async () => {
    const jwt = await token(claimsFor());
    // A string has no `kid`, so it can never match; the real key is still found.
    const withNoise = { keys: ["noise", { ...exported, kid, alg: "RS256" }] };
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(withNoise));
    expect(outcome.kind).toBe("verified");
  });
});

describe("a not-before claim that is not a number", () => {
  test("is ignored, because a token may omit it and a non-number states no time", async () => {
    // The string names a time far in the future. Read as a number it would be
    // NaN and never refuse; coerced in a comparison it would be huge and
    // always refuse. Only the type check ignores it, which is the rule.
    const jwt = await token(claimsFor({ nbf: String(Math.floor(Date.now() / 1000) + 999_999) }));
    const outcome = await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("verified");
  });
});

describe("a certs entry that is null", () => {
  test("is passed over without reading its fields", async () => {
    const jwt = await token(claimsFor());
    expect(
      await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom({ keys: [null] })),
    ).toEqual({ kind: "refused", refusal: { kind: "unknown-key", kid } });
  });
});

describe("the base64url decoder and the runtime's own base64", () => {
  test("a segment whose length is not a multiple of four decodes", () => {
    // These are the lengths that need padding in a strict base64 decoder. Bun's
    // atob is lenient, so this asserts the result rather than the mechanism.
    expect(new TextDecoder().decode(base64UrlToBytes("YQ"))).toBe("a");
    expect(new TextDecoder().decode(base64UrlToBytes("YWI"))).toBe("ab");
  });

  test("the last byte is included, so nothing is truncated", () => {
    const bytes = base64UrlToBytes("AQIDBA");
    expect([...bytes]).toEqual([1, 2, 3, 4]);
  });
});

describe("a certs entry that is an array", () => {
  test("is passed over, because an array can carry a key id and is still not a key", async () => {
    // An array can have a `kid` property assigned to it, so the object check is
    // what stops it being chosen as a key.
    const impostor: unknown[] & { kid?: string } = [];
    impostor.kid = kid;
    const jwt = await token(claimsFor());
    expect(
      await verifyAccess(requestWith(jwt), config, Date.now(), keysFrom({ keys: [impostor] })),
    ).toEqual({ kind: "refused", refusal: { kind: "unknown-key", kid } });
  });
});

describe("a signature that is not valid base64url", () => {
  test("is refused rather than thrown", async () => {
    const jwt = await token(claimsFor());
    const broken = `${jwt.slice(0, jwt.lastIndexOf(".") + 1)}***`;
    const outcome = await verifyAccess(requestWith(broken), config, Date.now(), keysFrom(jwks));
    expect(outcome.kind).toBe("refused");
  });
});

describe("a payload segment that is not valid base64url", () => {
  test("is refused as malformed", async () => {
    const head = base64Url(encoder.encode(JSON.stringify({ alg: "RS256", kid })));
    expect(
      await verifyAccess(requestWith(`${head}.***.***`), config, Date.now(), neverFetched),
    ).toEqual({
      kind: "refused",
      refusal: { kind: "malformed", reason: "expected JSON header and payload" },
    });
  });
});
