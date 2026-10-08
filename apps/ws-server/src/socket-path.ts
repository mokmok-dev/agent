import { Effect, Schema } from "effect";

export const configKey = "WS_SOCKET_PATH";

export const SocketPath = Schema.String.check(
  Schema.isPattern(/^\/[A-Za-z0-9._/-]+$/),
  Schema.isMaxLength(103),
).pipe(Schema.brand("SocketPath"));
export type SocketPath = typeof SocketPath.Type;

export class SocketPathError extends Schema.TaggedError<SocketPathError>()("SocketPathError", {
  input: Schema.String,
  cause: Schema.Unknown,
}) {}

export const decodeSocketPath = (input: string): Effect.Effect<SocketPath, SocketPathError> =>
  Schema.decodeEffect(SocketPath)(input).pipe(
    Effect.mapError((cause) => new SocketPathError({ input, cause })),
  );

/**
 * `requestPath` must start with "/". `ws` recovers the socket path as
 * `opts.path.split(":")[0]`.
 */
export const socketPathUrl = (path: string, requestPath: string): string =>
  `ws+unix://${path}:${requestPath}`;
