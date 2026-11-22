import { newState, type LinearOAuthConfig } from "@agent/adapters";
import { oauthConfigured, oauthStart, page, type LinearOAuthEnv } from "./routes.ts";
import type { LinearCredential } from "./linear-oauth-do.ts";

/**
 * What the Worker needs from the Linear authorization object. The Worker never
 * touches storage; it asks this port, and the platform class forwards to the
 * tested object.
 */
export interface LinearOAuthPort {
  /** Store a state for a login and return it. */
  start(): Promise<string>;
  /** Finish a login and return the stored credential. */
  complete(params: Record<string, string>, now: number): Promise<LinearCredential>;
}

/**
 * `GET /oauth/linear`: issue a state, store it, then redirect to Linear's
 * consent screen. A missing client id or secret is reported here, because a
 * consent screen with an empty client id can only fail at Linear.
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
  await port.start();
  return oauthStart(config, state);
}

/**
 * `GET /oauth/linear/callback`: finish the login. A success shows the workspace
 * and the token lifetime; a refusal shows Linear's own message with its status.
 */
export async function finishLogin(
  port: LinearOAuthPort,
  params: Record<string, string>,
  now: number,
): Promise<Response> {
  try {
    const credential = await port.complete(params, now);
    return page("Linear connected", describe(credential));
  } catch (error) {
    // An error thrown across a Durable Object RPC arrives as a rebuilt error
    // without its own fields, so the status is recovered from the message the
    // adapters give each refusal.
    const message = error instanceof Error ? error.message : String(error);
    return Response.json({ error: { message } }, { status: statusOf(message) });
  }
}

/**
 * The refusal's own status. A Durable Object RPC rebuilds a thrown error, so its
 * `status` field does not survive; the callbacks and the token endpoint are
 * client errors, which the message distinguishes from a bad response.
 */
function statusOf(message: string): number {
  return message.startsWith("linear token response") || message.startsWith("linear token endpoint")
    ? 502
    : 400;
}

function describe(credential: LinearCredential): string {
  const workspace = credential.organizationId ?? "the authorized workspace";
  const hours = Math.round(credential.expiresIn / 3600);
  return `The deployment can now act in ${workspace}. The access token lives ${hours} hours and is refreshed automatically. You can close this window.`;
}
