import { createServer } from "node:http";
import { statSync, unlinkSync } from "node:fs";

import { Effect } from "effect";
import { ServeError } from "effect/http/HttpServerError";
import * as HttpServer from "effect/http/HttpServer";
import * as Request from "effect/http/HttpServerRequest";
import * as Response from "effect/http/HttpServerResponse";
import * as Socket from "effect/socket/Socket";
import * as NodeHttpServer from "@effect/platform-node/NodeHttpServer";

import { handleConnection } from "./connection.js";
import { routeOf } from "./routes.js";
import { decodeSocketPath, SocketPathError } from "./socket-path.js";

export const removeStaleSocket = (path: string): Effect.Effect<void> =>
  Effect.sync(() => {
    if (statSync(path, { throwIfNoEntry: false })?.isSocket()) {
      unlinkSync(path);
    }
  }).pipe(Effect.ignore);

export interface ServeOptions {
  readonly socketPath: string;
}

export type ServeFailure = SocketPathError | ServeError;

const app = Effect.gen(function* () {
  const request = yield* Request.HttpServerRequest;
  const route = routeOf(request.method, request.url);
  if (route._tag === "Health") {
    return Response.text("ok");
  }
  if (route._tag === "NotFound") {
    return Response.empty({ status: 404 });
  }
  return yield* request.upgrade.pipe(
    Effect.matchEffect({
      onFailure: () => Effect.succeed(Response.empty({ status: 426 })),
      onSuccess: (socket: Socket.Socket) =>
        handleConnection(socket).pipe(Effect.as(Response.empty())),
    }),
  );
});

export const serve = (options: ServeOptions): Effect.Effect<void, ServeFailure> =>
  Effect.gen(function* () {
    const path = yield* decodeSocketPath(options.socketPath);
    yield* removeStaleSocket(path);
    const layer = NodeHttpServer.layer(() => createServer(), { path });
    yield* Effect.scoped(
      Effect.gen(function* () {
        const server = yield* HttpServer.HttpServer;
        yield* Effect.log(`listening on ${HttpServer.formatAddress(server.address)}`);
        yield* Effect.forkScoped(server.serve(app).pipe(Effect.andThen(Effect.never)));
        yield* Effect.never;
      }).pipe(Effect.provide(layer)),
    );
  });
