import {
  fetch as handle,
  replyFor,
  resolveGatewaySession,
  type Env,
  type Namespace,
} from "./worker.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { LinearOAuthObject } from "./linear-oauth-do.ts";
import { ProviderCredentialObject } from "./providers-do.ts";
import { DiskHost } from "./disk-host.ts";
import type { StorageHost } from "./storage.ts";
import { catalogOf, routingPolicyOf } from "./routing.ts";
import { frameText, handleSocketMessage, type GatewaySessions } from "./gateway-connection.ts";
import { helloOf } from "./gateway-plane.ts";
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
    new SessionObject(new DiskHost(join(root, "sessions", name)), {
      policy: routingPolicyOf(env),
      // The deployment pins its model onto the agent the policy names, so a
      // local run must do the same or it routes a model the stated provider
      // does not reach.
      catalog: catalogOf(env, routingPolicyOf(env).defaultAgent),
    });
  return {
    session: (sessionId) => sessions(sessionId).session(sessionId),
    turns: (sessionId) => sessions(sessionId).turns(sessionId),
    transition: (sessionId, turnId, update, now) =>
      sessions(sessionId).transition(sessionId, turnId, update, now),
    bind: (sessionId, acpSessionId, gatewayId, now) =>
      sessions(sessionId).bind(sessionId, acpSessionId, gatewayId, now),
    capability: (sessionId, capability, now) =>
      sessions(sessionId).capability(sessionId, capability, now),
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
    (host) =>
      new SessionObject(host, {
        policy: routingPolicyOf(env),
        catalog: catalogOf(env, routingPolicyOf(env).defaultAgent),
      }),
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

/**
 * One session object, by id, as the dev server needs it for a sweep.
 *
 * The deployed object is the same shape; here it is built on demand because a
 * disk host has no single object that outlives a request.
 */
function sessionObject(name: string): SessionObject {
  return new SessionObject(new DiskHost(join(root, "sessions", name)), {
    policy: routingPolicyOf(env),
    catalog: catalogOf(env, routingPolicyOf(env).defaultAgent),
  });
}

/**
 * Fail the expired turns and answer the thread they came from.
 *
 * The deployed object sends this reply from its sweep. The dev sweep has to
 * send the same one, or a local run shows a failed turn beside a silent thread
 * and the two paths disagree about what a person sees.
 */
async function sweep(name: string): Promise<void> {
  const object = sessionObject(name);
  const expired = (await object.expire(name, Date.now())) as {
    error?: string | null;
  }[];
  for (const turn of expired) {
    const session = await object.session(name);
    if (session === undefined) continue;
    await replyFor(
      linearReplies(
        env.LINEAR_OAUTH.getByName("linear") as LinearOAuthPort,
        undefined,
        process.env.LINEAR_GRAPHQL_URL,
      ),
      session.thread.threadId,
      { kind: "failed", error: turn.error ?? "no gateway claimed this turn" },
      Date.now(),
    );
  }
}

/**
 * Arm a one-shot sweep for the session's next claim deadline.
 *
 * The deployment arms a Durable Object alarm at this instant; a process has no
 * alarm, so it schedules a timer for the same one. Both run the same expiry
 * rule, so a local run and a deployment fail a turn at the same mark.
 */
const armed = new Map<string, ReturnType<typeof setTimeout>>();
function armDeadline(name: string): void {
  void sessionObject(name)
    .deadline(name, Date.now())
    .then((at) => {
      const existing = armed.get(name);
      if (existing !== undefined) clearTimeout(existing);
      armed.delete(name);
      if (at === undefined) return;
      armed.set(
        name,
        setTimeout(
          () => {
            armed.delete(name);
            void sweep(name).then(() => armDeadline(name));
          },
          Math.max(0, at - Date.now()),
        ),
      );
    });
}

/** What a connected gateway carries between messages. */
type SocketData = {
  sessionId:
    | string
    | undefined; /** The id this object really is, which differs from `auto` after resolution. */
  resolvedId: string | undefined;
  queue: Promise<void>;
};

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
  // The same driver the deployed object uses, so the two paths cannot drift.
  const gateway = {
    sessionId: socket.data.sessionId,
    send: (frame: string) => socket.send(frame),
  };
  // A gateway that asked for `auto` does not know the id it reached, so the
  // hello is answered with the resolved id before the driver sees it. Without
  // this the connection would try to serve a session literally called "auto".
  const hello = helloOf(parseJson(raw));
  if (hello === "auto" && socket.data.resolvedId !== undefined) {
    socket.send(frameText({ type: "hello", sessionId: socket.data.resolvedId }));
    gateway.sessionId = socket.data.resolvedId;
    socket.data.sessionId = socket.data.resolvedId;
    return;
  }
  await handleSocketMessage(gatewaySessions(), gateway, raw, Date.now(), replyToLinear);
  socket.data.sessionId = gateway.sessionId;
}
function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** The session a webhook's answer names, when it created or replayed a turn. */
function sessionIdOf(body: unknown): string | undefined {
  if (typeof body !== "object" || body === null) return undefined;
  const outcome = (body as { outcome?: unknown }).outcome;
  if (typeof outcome !== "object" || outcome === null) return undefined;
  const session = (outcome as { session?: unknown }).session;
  if (typeof session !== "object" || session === null) return undefined;
  const id = (session as { id?: unknown }).id;
  return typeof id === "string" && id !== "" ? id : undefined;
}

export function serve(port: number): ReturnType<typeof Bun.serve> {
  const server = Bun.serve<SocketData, never>({
    port,
    // The upgrade is answered from an async resolution, so the handshake is
    // taken here and the socket is accepted afterwards. The deployed Worker
    // takes the same step before handing the upgrade to the object.
    async fetch(request, server) {
      const url = new URL(request.url);
      if (url.pathname === "/gateway") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return Response.json({ error: "expected a WebSocket upgrade" }, { status: 426 });
        }
        const requested = url.searchParams.get("session");
        if (requested === null || requested === "") {
          return Response.json(
            { error: "name the session a gateway serves with ?session=<id>" },
            { status: 400 },
          );
        }
        // `auto` is resolved the same way the deployment resolves it, so a local
        // run reaches the session a live delegation created. Answering `auto`
        // literally would make the gateway serve a session that does not exist.
        const resolvedId = await resolveGatewaySession(env, requested);
        if (resolvedId === undefined) {
          return Response.json(
            { error: "no session is waiting for a gateway; delegate an issue first" },
            { status: 404 },
          );
        }
        const data: SocketData = {
          sessionId: undefined,
          resolvedId,
          queue: Promise.resolve(),
        };
        if (server.upgrade(request, { data })) return undefined;
        return Response.json({ error: "expected a WebSocket upgrade" }, { status: 426 });
      }
      return handle(request, env).then((response) => {
        // A delivery creates the turn, so this is the moment its deadline
        // starts. The deployment arms a Durable Object alarm here; a process
        // arms a timer, and the sweep is the same one.
        if (url.pathname.startsWith("/webhooks/")) {
          void response
            .clone()
            .json()
            .then((body: unknown) => {
              const sessionId = sessionIdOf(body);
              if (sessionId !== undefined) armDeadline(sessionId);
            });
        }
        return response;
      });
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
