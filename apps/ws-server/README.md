# @agent/ws-server

A WebSocket server that binds a Unix domain socket. One process, one socket file,
one WebSocket endpoint. The HTTP handshake, the frame codec, ping and pong, and
close codes come from the platform, so this package carries only the socket path
rule, the wire protocol, and the connection loop.

## Run it

```bash
pnpm typecheck                       # builds apps/ws-server/dist
WS_SOCKET_PATH=/run/agent.sock pnpm --filter @agent/ws-server start
```

The process logs the address it bound and runs until it is interrupted. On
shutdown it closes the listener and removes the socket file.

`WS_SOCKET_PATH` is the only configuration. It must be an absolute path, at most
103 bytes, and free of `:`, `?`, `#`, `%`, and whitespace. The colon rule is not
a style choice. The `ws` client recovers the socket path from a `ws+unix://` URL
by splitting on the first colon, so a path containing one cannot be dialed.

A socket file left behind by a killed process is removed before binding. Anything
else already at the path, such as a regular file, makes the bind fail and is left
alone.

## Endpoints

| Request                             | Response                     |
| ----------------------------------- | ---------------------------- |
| `GET /health`                       | `200 ok`                     |
| `GET /ws` with an upgrade header    | `101`, then WebSocket frames |
| `GET /ws` without an upgrade header | `426`                        |
| anything else                       | `404`                        |

## Wire protocol

One JSON document per text frame. A client sends one of:

```json
{"_tag":"Echo","text":"hello"}
{"_tag":"Ping","id":7}
```

The server answers `{"_tag":"Echo","text":"hello"}` for an echo,
`{"_tag":"Pong","id":7}` for a ping, and `{"_tag":"Rejected","reason":"invalid message"}`
for a frame that is not one of the two. A rejected frame leaves the connection
open.

## Dial it

The socket path and the request path travel in one URL. The `ws` package recovers
the socket path from the part before the colon and uses the rest as the request
path.

```ts
const socketPath = "/run/agent.sock";
const socket = new WebSocket(`ws+unix://${socketPath}:/ws`);
```

From Effect, `Socket.makeWebSocket` needs a constructor that understands
`ws+unix:`.

```ts
import * as NodeSocket from "@effect/platform-node/NodeSocket";
import * as Socket from "effect/socket/Socket";

const makeClient = (socketPath: string) =>
  Socket.makeWebSocket(`ws+unix://${socketPath}:/ws`).pipe(
    Effect.provide(NodeSocket.layerWebSocketConstructorWS),
  );
```

## Use it as a library

```ts
import { serve } from "@agent/ws-server";

await Effect.runPromise(serve({ socketPath: "/run/agent.sock" }));
```

`serve` decodes the path, binds the socket, and never completes. A bad path fails
with `SocketPathError` before anything touches the filesystem. A path that cannot
be bound fails with `ServeError`.

## Develop it

```bash
pnpm install
pnpm typecheck
pnpm test
pnpm mutate
pnpm exec oxlint
nix flake check
```
