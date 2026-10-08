import { Cause, Effect } from "effect";
import * as Socket from "effect/socket/Socket";

import { decodeClientMessage, encodeServerMessage, rejected, replyTo } from "./protocol.js";

export const handleConnection = (socket: Socket.Socket): Effect.Effect<void> =>
  Effect.scoped(
    Effect.gen(function* () {
      const writer = yield* socket.writer;
      const pull = yield* Socket.readerString(socket);
      yield* Effect.forever(
        pull.pipe(
          Effect.flatMap((frames) =>
            Effect.forEach(frames, (frame) =>
              decodeClientMessage(frame).pipe(
                Effect.map(replyTo),
                Effect.orElseSucceed(() => rejected),
                Effect.flatMap((message) => writer.write(encodeServerMessage(message))),
              ),
            ),
          ),
        ),
      );
    }),
  ).pipe(
    // A disconnect ends the pull with a `SocketError`. Reporting it and completing
    // makes the teardown a normal route completion, so the upgraded response is
    // what the node adapter acts on and no HTTP write reaches the socket.
    Effect.catchCause((cause) =>
      Effect.logDebug("websocket connection ended", Cause.squash(cause)),
    ),
  );
