import { Context, Effect, Layer, Schema } from "effect";
import type {
  NetworkRestrictionConfig,
  SandboxAskCallback,
  SandboxRuntimeConfig,
  WrapWithSandboxOptions,
} from "@anthropic-ai/sandbox-runtime";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

export interface WrappedArgv {
  readonly argv: ReadonlyArray<string>;
  readonly env: NodeJS.ProcessEnv;
}

export class PortInitializeError extends Schema.TaggedError<PortInitializeError>()(
  "PortInitializeError",
  {
    cause: Schema.Unknown,
  },
) {}

export class PortWrapError extends Schema.TaggedError<PortWrapError>()("PortWrapError", {
  command: Schema.String,
  cause: Schema.Unknown,
}) {}

export interface SandboxPort {
  readonly initialize: (
    config: SandboxRuntimeConfig,
    ask: SandboxAskCallback,
  ) => Effect.Effect<void, PortInitializeError>;
  readonly updateConfig: (config: SandboxRuntimeConfig) => void;
  readonly reset: () => Effect.Effect<void>;
  /**
   * `binShell` is srt's second parameter, the shell the wrapper is spawned
   * under. srt's own default is `/bin/bash`, which a host without a merged
   * `/usr` does not have, so the caller names one that exists.
   */
  readonly wrapArgv: (
    command: string,
    customConfig: Partial<SandboxRuntimeConfig>,
    binShell?: string,
  ) => Effect.Effect<WrappedArgv, PortWrapError>;
  readonly restrictions: () => NetworkRestrictionConfig;
}

export const SandboxPort: Context.Service<SandboxPort, SandboxPort> = Context.Service(
  "agent/sandbox-broker/SandboxPort",
);

export interface ManagerLike {
  readonly initialize: (
    runtimeConfig: SandboxRuntimeConfig,
    sandboxAskCallback?: SandboxAskCallback,
    enableLogMonitor?: boolean,
  ) => Promise<void>;
  readonly updateConfig: (newConfig: SandboxRuntimeConfig) => void;
  readonly reset: () => Promise<void>;
  readonly wrapWithSandboxArgv: (
    command: string,
    binShell?: string,
    customConfig?: Partial<SandboxRuntimeConfig>,
    abortSignal?: AbortSignal,
    cwd?: string,
    options?: WrapWithSandboxOptions,
  ) => Promise<{ readonly argv: Array<string>; readonly env: NodeJS.ProcessEnv }>;
  readonly getNetworkRestrictionConfig: () => NetworkRestrictionConfig;
}

export const managerPort = (manager: ManagerLike): SandboxPort => ({
  initialize: (config, ask) =>
    Effect.tryPromise({
      try: () => manager.initialize(config, ask),
      catch: (cause) => new PortInitializeError({ cause }),
    }),
  updateConfig: (config) => manager.updateConfig(config),
  reset: () => Effect.orDie(Effect.tryPromise(() => manager.reset())),
  wrapArgv: (command, customConfig, binShell) =>
    Effect.tryPromise({
      try: () => manager.wrapWithSandboxArgv(command, binShell, customConfig, undefined),
      catch: (cause) => new PortWrapError({ command, cause }),
    }),
  restrictions: () => manager.getNetworkRestrictionConfig(),
});

export const layerReal: Layer.Layer<SandboxPort> = Layer.effect(
  SandboxPort,
  Effect.sync(() => managerPort(SandboxManager)),
);
