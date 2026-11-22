import { newState, type LinearOAuthConfig } from "@agent/adapters";
import { oauthConfigured, oauthStart, page, type LinearOAuthEnv } from "./routes.ts";
import type { AccessToken, Completed, LinearCredential, LoginRefused } from "./linear-oauth-do.ts";

/**
 * What the Worker needs from the Linear authorization object. The Worker never
 * touches storage; it asks this port, and the platform class forwards to the
 * tested object.
 */
export interface LinearOAuthPort {
  /** Store this state, so the callback can prove the flow started here. */
  start(state: string): Promise<void>;
  /** Finish a login: the stored credential, or the refusal that ended it. */
  complete(params: Record<string, string>, now: number): Promise<Completed>;
  /** The stored credential, or undefined before the first login. */
  status(now: number): Promise<LinearCredential | undefined>;
  /** A usable token, renewing it first when it is inside the skew. */
  accessToken(now: number): Promise<AccessToken | undefined>;
}

/**
 * `GET /oauth/linear`: issue a state, store it, then redirect to Linear's
 * consent screen. A missing client id or secret is reported here, because a
 * consent screen with an empty client id can only fail at Linear.
 *
 * The state that goes into the URL is the one this function generated, and it
 * is the one stored, so the callback's copy matches.
 */
export async function startLogin(
  port: LinearOAuthPort,
  env: LinearOAuthEnv,
  config: LinearOAuthConfig,
  state: string = newState(),
): Promise<Response> {
  if (!oauthConfigured(env)) {
    return Response.json(
      { error: { message: "LINEAR_CLIENT_ID and LINEAR_CLIENT_SECRET must be set" } },
      { status: 503 },
    );
  }
  await port.start(state);
  return oauthStart(config, state);
}

/**
 * `GET /oauth/linear/callback`: finish the login. A success shows the workspace
 * and the token lifetime; a refusal shows Linear's own message with its status.
 */ export async function finishLogin(
  port: LinearOAuthPort,
  params: Record<string, string>,
  now: number,
): Promise<Response> {
  const answer = await port.complete(params, now);
  if (answer.kind !== "completed") return refusalPage(answer);
  return page("Linear connected", describe(answer.credential));
}

/** A login the object refused, answered with the status the refuser chose. */
function refusalPage(refusal: LoginRefused): Response {
  return Response.json({ error: { message: refusal.message } }, { status: statusOf(refusal) });
}

/**
 * The status a refusal answers with. A callback this object rejected carries
 * its own 400; a Linear answer it could not use is a 502, because the fault is
 * upstream. The union says which, so nothing is inferred from a message.
 */
function statusOf(refusal: LoginRefused): number {
  return refusal.kind === "rejected" ? refusal.status : 502;
}

function describe(credential: LinearCredential): string {
  const workspace = credential.organizationId ?? "the authorized workspace";
  const hours = Math.round(credential.expiresIn / 3600);
  return `The deployment can now act in ${workspace}. The access token lives ${hours} hours and is refreshed automatically. You can close this window.`;
}

/**
 * `GET /oauth/linear/status`: what the deployment holds, without the tokens.
 * The token values never leave the object, so this is safe to open in a browser.
 */
export async function loginStatus(port: LinearOAuthPort, now: number): Promise<Response> {
  const credential = await port.status(now);
  if (credential === undefined) {
    return Response.json({ connected: false }, { status: 404 });
  }
  return Response.json({
    connected: true,
    organizationId: credential.organizationId,
    scope: credential.scope,
    expiresAt: new Date(credential.issuedAt + credential.expiresIn * 1000).toISOString(),
    renewsInSeconds: Math.round((credential.issuedAt + credential.expiresIn * 1000 - now) / 1000),
  });
}

/**
 * `GET /providers`: which providers this deployment may run and which have a
 * credential. A provider name and its credential type are safe to show; the
 * credential itself never appears here.
 */
export function providerStatus(
  configured: readonly string[],
  stored: readonly { providerId: string; type: string }[],
): Response {
  const held = new Map(stored.map((entry) => [entry.providerId, entry.type]));
  return Response.json({
    providers: configured.map((provider) => ({
      provider,
      credential: held.get(provider) ?? null,
    })),
  });
}
