import { Effect, Schema } from "effect";

export const clientMessage = Schema.Union([
  Schema.TaggedStruct("Echo", { text: Schema.String }),
  Schema.TaggedStruct("Ping", { id: Schema.Int }),
]);
export type ClientMessage = typeof clientMessage.Type;

export const serverMessage = Schema.Union([
  Schema.TaggedStruct("Echo", { text: Schema.String }),
  Schema.TaggedStruct("Pong", { id: Schema.Int }),
  Schema.TaggedStruct("Rejected", { reason: Schema.String }),
]);
export type ServerMessage = typeof serverMessage.Type;

export class ProtocolError extends Schema.TaggedError<ProtocolError>()("ProtocolError", {
  raw: Schema.String,
  cause: Schema.Unknown,
}) {}

export const decodeClientMessage = (raw: string): Effect.Effect<ClientMessage, ProtocolError> =>
  Schema.decodeEffect(Schema.fromJsonString(clientMessage))(raw).pipe(
    Effect.mapError((cause) => new ProtocolError({ raw, cause })),
  );

export const encodeServerMessage = (message: ServerMessage): string =>
  Schema.encodeSync(Schema.fromJsonString(serverMessage))(message);

export const replyTo = (message: ClientMessage): ServerMessage =>
  message._tag === "Echo" ? { _tag: "Echo", text: message.text } : { _tag: "Pong", id: message.id };

export const rejected: ServerMessage = { _tag: "Rejected", reason: "invalid message" };
