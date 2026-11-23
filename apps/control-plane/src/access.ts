/**
 * Cloudflare Access, checked where it is needed.
 *
 * Access runs in front of the Worker and turns away anyone without a session,
 * but it protects the whole hostname. That is wrong for this deployment: Linear
 * delivers webhooks to a signature-verified path, and a browser redirect cannot
 * present a signature, so protecting everything broke the integration. The
 * Worker therefore verifies Access itself, and only on the paths a person or a
 * machine acts on.
 *
 * Two credentials reach the origin:
 *
 * - A browser sends `Cf-Access-Jwt-Assertion`, a JWT Access signs for this
 *   application. It is verified against the team's public keys.
 * - A machine sends `CF-Access-Client-Id` and `CF-Access-Client-Secret` (a
 *   service token). Access exchanges them for a JWT before the request reaches
 *   the Worker, so the Worker only ever sees the JWT.
 *
 * The JWT is checked for signature, issuer, audience, and expiry. Every field
 * is load-bearing: a signature without an audience check accepts a token minted
 * for any application in the team, and without an expiry check accepts one
 * forever.
 */

/** What the deployment states about its Access application. */
export type AccessConfig = {
  /** The team domain, as in `https://<team>.cloudflareaccess.com`. */
  teamDomain: string;
  /** The application's audience tag, which the JWT's `aud` must contain. */
  audience: string;
};

export type AccessEnv = {
  /** The team domain; unset means this deployment does not use Access. */
  ACCESS_TEAM_DOMAIN?: string;
  /** The application's audience tag. Both must be set for Access to be checked. */
  ACCESS_AUD?: string;
};

/** The Access settings a deployment states, or `undefined` when it does not use it. */
export function accessConfigOf(env: AccessEnv): AccessConfig | undefined {
  const teamDomain = (env.ACCESS_TEAM_DOMAIN ?? "").trim();
  const audience = (env.ACCESS_AUD ?? "").trim();
  if (teamDomain === "" || audience === "") return undefined;
  return { teamDomain, audience };
}

/** The claims this module reads. Access sets more; these are the ones it checks. */
type AccessClaims = {
  iss: string;
  aud: string[] | string;
  exp: number;
  iat?: number;
  nbf?: number;
  common_name?: string;
  email?: string;
  sub?: string;
};

export type AccessRefused =
  | { kind: "no-token" }
  | { kind: "malformed"; reason: string }
  | { kind: "unknown-key"; kid: string }
  | { kind: "bad-signature" }
  | { kind: "wrong-issuer"; issuer: string }
  | { kind: "wrong-audience" }
  | { kind: "expired" }
  | { kind: "keys-unavailable"; status: number };

export type AccessVerified = {
  /** Who Access says this is: a service token's name, or a person's email. */
  subject: string;
  /** The claims, which a caller may read for its own purpose. */
  claims: AccessClaims;
};

export type AccessOutcome =
  { kind: "verified"; value: AccessVerified } | { kind: "refused"; refusal: AccessRefused };

/** The header Access sets on a request it has admitted. */
export const ACCESS_JWT_HEADER = "cf-access-jwt-assertion";

/** The keys Access signs with, which rotate, so they are read per request. */
const CERTS_PATH = "/cdn-cgi/access/certs";

type Jwk = { kid?: string; kty?: string; alg?: string; n?: string; e?: string; use?: string };

/**
 * Verify the Access JWT on a request.
 *
 * A deployment that states no Access settings trusts its callers, which is what
 * a local run does; the platform's own network boundary is the whole check
 * there. `fetch` is injectable so a test drives real signing keys without
 * reaching the network.
 */
export async function verifyAccess(
  request: Request,
  config: AccessConfig,
  now: number,
  fetchJson: (url: string) => Promise<{ status: number; body: unknown }> = fetchJsonOf,
): Promise<AccessOutcome> {
  const token = request.headers.get(ACCESS_JWT_HEADER);
  // `Headers` already trims the value, so a whitespace-only header arrives as
  // the empty string and needs no separate check.
  if (token === null || token === "") return refuse({ kind: "no-token" });

  const parts = token.split(".");
  if (parts.length !== 3) return refuse({ kind: "malformed", reason: "expected three parts" });
  const [rawHeader, rawPayload, rawSignature] = parts as [string, string, string];

  const header = decodeJson(rawHeader);
  const claims = decodeJson(rawPayload);
  if (header === undefined || claims === undefined) {
    return refuse({ kind: "malformed", reason: "expected JSON header and payload" });
  }
  const kid = (header as { kid?: unknown }).kid;
  if (typeof kid !== "string" || kid === "") {
    return refuse({ kind: "malformed", reason: "no key id in the header" });
  }
  const alg = (header as { alg?: unknown }).alg;
  // Only RSA is accepted: the team's keys are RSA, and accepting the token's own
  // stated algorithm is how a token chooses a weaker one than the key intends.
  if (alg !== "RS256")
    return refuse({ kind: "malformed", reason: `unsupported alg ${String(alg)}` });

  const keys = await fetchJson(`https://${config.teamDomain}${CERTS_PATH}`);
  if (keys.status !== 200) return refuse({ kind: "keys-unavailable", status: keys.status });
  const jwk = findKey(keys.body, kid);
  if (jwk === undefined) return refuse({ kind: "unknown-key", kid });

  const verified = await verifySignature(jwk, `${rawHeader}.${rawPayload}`, rawSignature);
  if (!verified) return refuse({ kind: "bad-signature" });

  const typed = claims as Partial<AccessClaims>;
  const issuer = typeof typed.iss === "string" ? typed.iss : "";
  if (issuer !== `https://${config.teamDomain}`) {
    return refuse({ kind: "wrong-issuer", issuer });
  }
  if (!audienceMatches(typed.aud, config.audience)) return refuse({ kind: "wrong-audience" });
  if (typeof typed.exp !== "number" || typed.exp * 1000 <= now) return refuse({ kind: "expired" });
  // `nbf` and `iat` are optional; a token that carries one must not be used
  // before it says it is valid.
  if (typeof typed.nbf === "number" && typed.nbf * 1000 > now) return refuse({ kind: "expired" });

  return {
    kind: "verified",
    value: {
      subject: subjectOf(typed),
      claims: typed as AccessClaims,
    },
  };
}

/** The identity a verified token names, in the most specific form it carries. */
function subjectOf(claims: Partial<AccessClaims>): string {
  // A service token's `common_name` is the name an operator gave it, which is
  // what makes a log line say which machine connected.
  if (typeof claims.common_name === "string" && claims.common_name !== "")
    return claims.common_name;
  if (typeof claims.email === "string" && claims.email !== "") return claims.email;
  return typeof claims.sub === "string" ? claims.sub : "";
}

function audienceMatches(aud: string[] | string | undefined, expected: string): boolean {
  if (typeof aud === "string") return aud === expected;
  if (!Array.isArray(aud)) return false;
  return aud.includes(expected);
}

function refuse(refusal: AccessRefused): AccessOutcome {
  return { kind: "refused", refusal };
}

/** The key `kid` names, from a certs document shaped `{ keys: [...] }`. */
function findKey(body: unknown, kid: string): Jwk | undefined {
  const keys = (body as { keys?: unknown }).keys;
  if (!Array.isArray(keys)) return undefined;
  const found = keys.find(
    (candidate): candidate is Jwk =>
      // A plain object, not any object: an array can carry a `kid` property and
      // would then be chosen as a key, which fails to import and refuses a
      // token the team did sign.
      typeof candidate === "object" &&
      candidate !== null &&
      !Array.isArray(candidate) &&
      (candidate as { kid?: unknown }).kid === kid,
  );
  return found;
}

/**
 * The RSA signature, over the token's first two parts.
 *
 * `RSA` and `RSASSA-PKCS1-v1_5` are the same thing: Access signs with this and
 * names the key `RSA` in its JWKS. A mismatch here is what a forged token fails.
 */
async function verifySignature(jwk: Jwk, signed: string, signature: string): Promise<boolean> {
  // `findKey` chose this key by its id, and a certs document may publish an
  // entry with no material; importing that throws, so the token is refused
  // rather than the request failing.
  try {
    const key = await crypto.subtle.importKey(
      "jwk",
      { kty: "RSA", n: jwk.n, e: jwk.e, alg: "RS256", ext: true } as JsonWebKey,
      { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
      false,
      ["verify"],
    );
    return await crypto.subtle.verify(
      "RSASSA-PKCS1-v1_5",
      key,
      base64UrlToBytes(signature),
      new TextEncoder().encode(signed),
    );
  } catch {
    return false;
  }
}

/** A base64url segment as bytes, which is what a signature is. */
export function base64UrlToBytes(text: string): Uint8Array {
  const padded = text.replace(/-/g, "+").replace(/_/g, "/");
  const pad = padded.length % 4 === 0 ? "" : "=".repeat(4 - (padded.length % 4));
  const binary = atob(padded + pad);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** A base64url JSON segment, or `undefined` when it is not JSON. */
function decodeJson(text: string): unknown {
  try {
    const bytes = base64UrlToBytes(text);
    return JSON.parse(new TextDecoder().decode(bytes)) as unknown;
  } catch {
    return undefined;
  }
}

/** The certs document, fetched live because the keys rotate. */
async function fetchJsonOf(url: string): Promise<{ status: number; body: unknown }> {
  try {
    const response = await fetch(url);
    if (!response.ok) return { status: response.status, body: undefined };
    return { status: response.status, body: (await response.json()) as unknown };
  } catch {
    // An unreachable team domain is a refusal, not a crash: the caller treats it
    // exactly as a missing key.
    return { status: 503, body: undefined };
  }
}
