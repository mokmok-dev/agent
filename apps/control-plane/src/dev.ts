import { fetch as handle, replyFor, type Env, type Namespace } from "./worker.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { LinearOAuthObject } from "./linear-oauth-do.ts";
import { ProviderCredentialObject } from "./providers-do.ts";
import { DiskHost } from "./disk-host.ts";
import type { StorageHost } from "./storage.ts";
import { routingPolicyOf } from "./routing.ts";
import { onMessage, type GatewaySessions } from "./gateway-connection.ts";
import { linearReplies } from "./linear-reply.ts";
import type { LinearOAuthPort } from "./linear-oauth-port.ts";
import { join } from "node:path";

function namespace(build: (host: StorageHost) => unknown, root: string): Namespace {
  return { getByName: (name: string) => build(new DiskHost(join(root, name))) };
}

const root = process.env.STATE_DIR ?? ".agent-state";

/**
 * The gateway endpoint the VM connects to.
 *
 * A gateway announces the session it serves with a `hello` frame, so one URL
 * serves every gateway. The control plane sends that session's queued turn and
 * applies whatever the gateway reports back, which is what turns a Linear
 * delivery into a run on the VM.
 */
function gatewaySessions(): GatewaySessions {
  const sessions = (name: string) =>
    new SessionObject(new DiskHost(join(root, "sessions", name)), { policy: routingPolicyOf(env) });
  return {
    session: (sessionId) => sessions(sessionId).session(sessionId),
    turns: (sessionId) => sessions(sessionId).turns(sessionId),
    transition: (sessionId, turnId, update, now) =>
      sessions(sessionId).transition(sessionId, turnId, update, now),
    bind: (sessionId, acpSessionId, gatewayId, now) =>
      sessions(sessionId).bind(sessionId, acpSessionId, gatewayId, now),
  };
}

/**
 * Answer the thread a settled turn belongs to. The reply goes through the same
 * Linear port a delivery uses, so a VM run and a sandbox run reach the person
 * the same way.
 */
async function replyToLinear(step: {
  sessionId: string;
  replyTo: string;
  text?: string;
  error?: string;
}): Promise<void> {
  // `LINEAR_GRAPHQL_URL` lets a local run observe the reply without
  // contacting Linear; a real deployment leaves it unset.
  const endpoint = process.env.LINEAR_GRAPHQL_URL;
  const port = linearReplies(
    env.LINEAR_OAUTH.getByName("linear") as LinearOAuthPort,
    undefined,
    endpoint,
  );
  const run =
    step.text !== undefined
      ? { kind: "answered", text: step.text }
      : { kind: "failed", error: step.error ?? "no reason given" };
  await replyFor(port, step.replyTo, run, Date.now());
}

export const env: Env = {
  SLACK_SIGNING_SECRET: process.env.SLACK_SIGNING_SECRET ?? "dev-slack-secret",
  SLACK_BOT_USER_ID: process.env.SLACK_BOT_USER_ID ?? "UBOT",
  DISCORD_PUBLIC_KEY: process.env.DISCORD_PUBLIC_KEY ?? "",
  LINEAR_WEBHOOK_SECRET: process.env.LINEAR_WEBHOOK_SECRET ?? "dev-linear-secret",
  LINEAR_CLIENT_ID: process.env.LINEAR_CLIENT_ID ?? "",
  LINEAR_CLIENT_SECRET: process.env.LINEAR_CLIENT_SECRET ?? "",
  LINEAR_REDIRECT_URI:
    process.env.LINEAR_REDIRECT_URI ?? "http://localhost:8787/oauth/linear/callback",
  ...(process.env.AGENT_DEFAULT === undefined ? {} : { AGENT_DEFAULT: process.env.AGENT_DEFAULT }),
  ...(process.env.AGENT_RUNTIMES === undefined
    ? {}
    : { AGENT_RUNTIMES: process.env.AGENT_RUNTIMES }),
  ...(process.env.AGENT_AVAILABLE_RUNTIMES === undefined
    ? {}
    : { AGENT_AVAILABLE_RUNTIMES: process.env.AGENT_AVAILABLE_RUNTIMES }),
  ...(process.env.PI_PROVIDERS === undefined ? {} : { PI_PROVIDERS: process.env.PI_PROVIDERS }),
  ...(process.env.PI_DEFAULT_MODEL === undefined
    ? {}
    : { PI_DEFAULT_MODEL: process.env.PI_DEFAULT_MODEL }),
  SESSIONS: namespace(
    (host) => new SessionObject(host, { policy: routingPolicyOf(env) }),
    join(root, "sessions"),
  ),
  THREAD_INDEX: namespace((host) => new ThreadIndexObject(host), join(root, "index")),
  LINEAR_OAUTH: namespace(
    (host) =>
      new LinearOAuthObject(host, {
        clientId: process.env.LINEAR_CLIENT_ID ?? "",
        clientSecret: process.env.LINEAR_CLIENT_SECRET ?? "",
        redirectUri: process.env.LINEAR_REDIRECT_URI ?? "",
      }),
    join(root, "linear"),
  ),
  PROVIDERS: namespace((host) => new ProviderCredentialObject(host), join(root, "providers")),
};

/** What a connected gateway carries between messages. */
type SocketData = { sessionId: string | undefined; queue: Promise<void> };

/**
 * One message from a gateway, handled on its own.
 *
 * Messages on one connection are serialized by the caller, so a bind and a
 * completed event that arrive together cannot both read the session as it was
 * before the bind.
 */
async function handleGatewayMessage(
  socket: { data: SocketData; send: (frame: string) => void },
  raw: string,
): Promise<void> {
  const step = await onMessage(
    gatewaySessions(),
    { sessionId: socket.data.sessionId },
    raw,
    Date.now(),
  );
  socket.data.sessionId = step.connection.sessionId;
  for (const frame of step.send) socket.send(frame);
  if (step.settled !== undefined) await replyToLinear(step.settled);
}

export function serve(port: number): ReturnType<typeof Bun.serve> {
  const server = Bun.serve<SocketData, never>({
    port,
    fetch: (request, server) => {
      if (new URL(request.url).pathname === "/gateway") {
        const data: SocketData = { sessionId: undefined, queue: Promise.resolve() };
        if (server.upgrade(request, { data })) return undefined;
        return Response.json({ error: "expected a WebSocket upgrade" }, { status: 426 });
      }
      return handle(request, env);
    },
    websocket: {
      message(socket, raw) {
        socket.data.queue = socket.data.queue.then(() => handleGatewayMessage(socket, String(raw)));
      },
    },
  });
  console.log(`control plane listening on http://localhost:${port}`);
  console.log(`state in ${root}; point the gateway at ws://localhost:${port}/gateway`);
  return server;
}

if (import.meta.main) serve(Number(process.env.PORT ?? 8787));
