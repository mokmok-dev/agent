import { Config, Effect } from "effect";
import * as NodeRuntime from "@effect/platform-node/NodeRuntime";

import { serve } from "./server.js";
import { configKey } from "./socket-path.js";

const program = Effect.gen(function* () {
  const socketPath = yield* Config.String(configKey);
  yield* serve({ socketPath });
});

NodeRuntime.runMain(program);
