import { describe, expect, test } from "bun:test";
import {
  AgentMethod,
  ClientMethod,
  PROTOCOL_VERSION,
  STOP_REASONS,
  TOOL_KINDS,
  TOOL_STATUSES,
  PERMISSION_KINDS,
  PLAN_STATUSES,
  emptyMcpServers,
  offeredModels,
  agentCapabilityOf,
  offersModel,
  modelValueFor,
  decodeInitializeResult,
  decodeNewSessionResult,
  decodeLoadSessionResult,
  decodeSetConfigOptionResult,
} from "./types.ts";

describe("wire names", () => {
  test("agent methods match ACP v1", () => {
    expect(AgentMethod).toEqual({
      Initialize: "initialize",
      Authenticate: "authenticate",
      NewSession: "session/new",
      LoadSession: "session/load",
      Prompt: "session/prompt",
      SetMode: "session/set_mode",
      SetConfigOption: "session/set_config_option",
      Cancel: "session/cancel",
    });
  });

  test("client methods match ACP v1", () => {
    expect(ClientMethod).toEqual({
      SessionUpdate: "session/update",
      RequestPermission: "session/request_permission",
      ReadTextFile: "fs/read_text_file",
      WriteTextFile: "fs/write_text_file",
      CreateTerminal: "terminal/create",
      TerminalOutput: "terminal/output",
      WaitForTerminalExit: "terminal/wait_for_exit",
      KillTerminal: "terminal/kill",
      ReleaseTerminal: "terminal/release",
    });
  });

  test("the client speaks protocol version 1", () => {
    expect(PROTOCOL_VERSION).toBe(1);
  });

  test("every enum lists exactly the spec values", () => {
    expect(STOP_REASONS).toEqual([
      "end_turn",
      "max_tokens",
      "max_turn_requests",
      "refusal",
      "cancelled",
    ]);
    expect(TOOL_KINDS).toEqual([
      "read",
      "edit",
      "delete",
      "move",
      "search",
      "execute",
      "think",
      "fetch",
      "other",
    ]);
    expect(TOOL_STATUSES).toEqual(["pending", "in_progress", "completed", "failed"]);
    expect(PERMISSION_KINDS).toEqual([
      "allow_once",
      "allow_always",
      "reject_once",
      "reject_always",
    ]);
    expect(PLAN_STATUSES).toEqual(["pending", "in_progress", "completed"]);
  });

  test("no wire name is empty and every name is unique within its group", () => {
    for (const group of [
      Object.values(AgentMethod),
      Object.values(ClientMethod),
      STOP_REASONS,
      TOOL_KINDS,
      TOOL_STATUSES,
      PERMISSION_KINDS,
      PLAN_STATUSES,
    ]) {
      expect(new Set(group).size).toBe(group.length);
      for (const name of group) expect(name.length).toBeGreaterThan(0);
    }
  });
});

describe("the empty mcpServers shape", () => {
  test("an array-shaped agent receives an empty list", () => {
    // `grok agent stdio` refuses a record and accepts only a list.
    const sent = emptyMcpServers("array");
    expect(Array.isArray(sent)).toBe(true);
    expect(sent).toEqual([]);
  });

  test("a record-shaped agent receives an empty object", () => {
    // `opencode2 acp` refuses a list and accepts only a record.
    const sent = emptyMcpServers("record");
    expect(Array.isArray(sent)).toBe(false);
    expect(sent).toEqual({});
  });
});

describe("the models an agent offers", () => {
  // These fixtures are the shapes `grok agent stdio` and `opencode2 acp`
  // actually returned from `session/new`, so the adapter reads the real field
  // either way.
  test("a settings entry whose category is model wins, because it is the field an agent reads", () => {
    const offered = offeredModels({
      sessionId: "s",
      models: {
        currentModelId: "from-models",
        availableModels: [{ modelId: "from-models", name: "x" }],
      },
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model",
          type: "select",
          currentValue: "ollama-cloud/deepseek-v4.1-flash",
          options: [
            { value: "ollama-cloud/deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash" },
            { value: "opencode/tidepool-alpha-free", name: "Tidepool" },
          ],
        },
        { id: "mode", name: "Session Mode", category: "mode", currentValue: "build" },
      ],
    });
    expect(offered).toEqual({
      currentModelId: "ollama-cloud/deepseek-v4.1-flash",
      availableModels: [
        { modelId: "ollama-cloud/deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash" },
        { modelId: "opencode/tidepool-alpha-free", name: "Tidepool" },
      ],
    });
  });

  test("a model list stated in `models` is read when there is no settings entry", () => {
    expect(
      offeredModels({
        sessionId: "s",
        models: {
          currentModelId: "deepseek-v4.1-flash",
          availableModels: [{ modelId: "deepseek-v4.1-flash", name: "DeepSeek" }],
        },
      }),
    ).toEqual({
      currentModelId: "deepseek-v4.1-flash",
      availableModels: [{ modelId: "deepseek-v4.1-flash", name: "DeepSeek" }],
    });
  });

  test("a settings list with no model entry states no models, rather than guessing", () => {
    expect(
      offeredModels({
        sessionId: "s",
        configOptions: [{ id: "mode", name: "Mode", category: "mode", currentValue: "build" }],
      }),
    ).toBeUndefined();
  });

  test("a model entry with no options states no models, because it names none to choose", () => {
    expect(
      offeredModels({
        sessionId: "s",
        configOptions: [{ id: "model", name: "Model", category: "model", currentValue: "m" }],
      }),
    ).toBeUndefined();
  });

  test("an option with no stated current model uses the first one it lists", () => {
    expect(
      offeredModels({
        sessionId: "s",
        configOptions: [
          { id: "model", name: "Model", category: "model", options: [{ value: "a", name: "A" }] },
        ],
      }),
    ).toEqual({ currentModelId: "a", availableModels: [{ modelId: "a", name: "A" }] });
  });
});

describe("what an agent said it can run", () => {
  test("the capability names the agent, its current model, and every model it listed", () => {
    expect(
      agentCapabilityOf(
        { protocolVersion: 1, agentInfo: { name: "OpenCode", version: "2.3.0" } },
        {
          sessionId: "s",
          configOptions: [
            {
              id: "model",
              name: "Model",
              category: "model",
              currentValue: "b",
              options: [
                { value: "a", name: "A" },
                { value: "b", name: "B" },
              ],
            },
          ],
        },
      ),
    ).toEqual({
      agent: { name: "OpenCode", version: "2.3.0" },
      currentModelId: "b",
      availableModels: ["a", "b"],
      statesModels: true,
    });
  });

  test("an agent that states no models has an empty list and says so", () => {
    expect(agentCapabilityOf({ protocolVersion: 1 }, { sessionId: "s" })).toEqual({
      availableModels: [],
      statesModels: false,
    });
  });

  test("a session that was never opened reports no models and says so", () => {
    expect(agentCapabilityOf({ protocolVersion: 1 }, undefined)).toEqual({
      availableModels: [],
      statesModels: false,
    });
  });

  test("an offered model is one the agent listed, and only when it listed any", () => {
    const capability = {
      availableModels: ["a"],
      statesModels: true,
    };
    expect(offersModel(capability, "a")).toBe(true);
    expect(offersModel(capability, "b")).toBe(false);
    // An agent that stated no list says nothing about what it can run, so a
    // caller must not read that silence as "this model works".
    expect(offersModel({ availableModels: [], statesModels: false }, "a")).toBe(false);
  });
});

describe("choosing the value an agent lists", () => {
  test("a provider-qualified id is preferred when the agent lists it", () => {
    // opencode2 lists `ollama-cloud/deepseek-v4.1-flash`, so that is the value
    // its own settings accept.
    expect(
      modelValueFor(["ollama-cloud/deepseek-v4.1-flash", "deepseek-v4.1-flash"], {
        provider: "ollama-cloud",
        modelId: "deepseek-v4.1-flash",
      }),
    ).toBe("ollama-cloud/deepseek-v4.1-flash");
  });

  test("a bare id is used when the agent lists only that", () => {
    // grok lists `grok-4.6` with no provider prefix.
    expect(modelValueFor(["grok-4.6"], { provider: "xai", modelId: "grok-4.6" })).toBe("grok-4.6");
  });

  test("a model the agent does not list has no value", () => {
    expect(modelValueFor(["other"], { provider: "xai", modelId: "grok-4.6" })).toBeUndefined();
    expect(modelValueFor([], { provider: "xai", modelId: "grok-4.6" })).toBeUndefined();
  });
});

describe("the decoders an agent's answer goes through", () => {
  test("agentInfo is read when the agent states it and absent when it does not", () => {
    const withInfo = decodeInitializeResult({
      protocolVersion: 1,
      agentInfo: { name: "OpenCode", version: "2.3.0" },
    });
    expect(withInfo).toEqual({
      kind: "decoded",
      value: { protocolVersion: 1, agentInfo: { name: "OpenCode", version: "2.3.0" } },
    });
    // A malformed agentInfo is dropped rather than failing the whole handshake.
    expect(decodeInitializeResult({ protocolVersion: 1, agentInfo: { name: "x" } })).toEqual({
      kind: "decoded",
      value: { protocolVersion: 1 },
    });
  });

  test("a session's config options and models are read, and a malformed one is dropped", () => {
    const decoded = decodeNewSessionResult({
      sessionId: "s",
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model",
          currentValue: "a",
          options: [{ value: "a", name: "A", description: "the first" }],
        },
        { id: "mode", name: "Mode" },
      ],
      models: { currentModelId: "a", availableModels: [{ modelId: "a", name: "A" }] },
    });
    if (decoded.kind !== "decoded") throw new Error("expected a decoded session");
    expect(decoded.value.configOptions?.[0]).toEqual({
      id: "model",
      name: "Model",
      category: "model",
      currentValue: "a",
      options: [{ value: "a", name: "A", description: "the first" }],
    });
    expect(decoded.value.models).toEqual({
      currentModelId: "a",
      availableModels: [{ modelId: "a", name: "A" }],
    });
    expect(decodeNewSessionResult({ sessionId: "s", models: { availableModels: [] } })).toEqual({
      kind: "decoded",
      value: { sessionId: "s" },
    });
  });

  test("a setting reply is read for the options it returns", () => {
    expect(
      decodeSetConfigOptionResult({
        configOptions: [{ id: "model", name: "Model", currentValue: "grok-4.6" }],
      }),
    ).toEqual({
      kind: "decoded",
      value: { configOptions: [{ id: "model", name: "Model", currentValue: "grok-4.6" }] },
    });
    expect(decodeSetConfigOptionResult({})).toEqual({ kind: "decoded", value: {} });
  });

  test("a description the agent omits is absent rather than an empty string", () => {
    const offered = offeredModels({
      sessionId: "s",
      configOptions: [
        { id: "model", name: "Model", category: "model", options: [{ value: "a", name: "A" }] },
      ],
    });
    expect(offered?.availableModels).toEqual([{ modelId: "a", name: "A" }]);
  });
});

describe("reading an agent's model list", () => {
  test("a setting that is not a model is passed over for the model one", () => {
    // An agent lists mode and effort alongside its model, so picking the first
    // setting would name a model that is not a model.
    const offered = offeredModels({
      sessionId: "s",
      configOptions: [
        { id: "mode", name: "Mode", category: "mode", currentValue: "build" },
        {
          id: "model",
          name: "Model",
          category: "model",
          currentValue: "b",
          options: [{ value: "b", name: "B" }],
        },
      ],
    });
    expect(offered?.currentModelId).toBe("b");
  });

  test("a listed model keeps the description the agent gave it", () => {
    const offered = offeredModels({
      sessionId: "s",
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model",
          options: [{ value: "a", name: "A", description: "the agent's own words" }],
        },
      ],
    });
    expect(offered?.availableModels).toEqual([
      { modelId: "a", name: "A", description: "the agent's own words" },
    ]);
  });
});

describe("what a stated capability keeps", () => {
  test("an agent that states no identity is not given a made-up one", () => {
    const capability = agentCapabilityOf({ protocolVersion: 1 }, { sessionId: "s" });
    expect("agent" in capability).toBe(false);
  });

  test("a model the agent lists without a description has none in the capability", () => {
    const capability = agentCapabilityOf(
      { protocolVersion: 1 },
      {
        sessionId: "s",
        configOptions: [
          { id: "model", name: "Model", category: "model", options: [{ value: "a", name: "A" }] },
        ],
      },
    );
    expect(capability.availableModels).toEqual(["a"]);
  });
});

describe("an agent that names no models", () => {
  test("a model check on such an agent is false, even for an id it happens to carry", () => {
    // `statesModels: false` means the agent said nothing, so no model can be
    // confirmed against it; reading its empty list as agreement would run a
    // model it never named.
    expect(offersModel({ availableModels: ["a"], statesModels: false }, "a")).toBe(false);
  });
});

describe("what a load reply carries", () => {
  test("every field of a load reply is read, so none is silently dropped", () => {
    // grok answers a load with the models and settings it would answer a new
    // session with. A decoder that dropped one of them would leave the caller
    // checking a routed model against nothing.
    const decoded = decodeLoadSessionResult({
      sessionId: "acp-stored",
      modes: { currentModeId: "build", availableModes: [{ id: "build", name: "Build" }] },
      models: { currentModelId: "m", availableModels: [{ modelId: "m", name: "M" }] },
      configOptions: [{ id: "model", name: "Model", currentValue: "m" }],
    });
    expect(decoded).toEqual({
      kind: "decoded",
      value: {
        sessionId: "acp-stored",
        modes: { currentModeId: "build", availableModes: [{ id: "build", name: "Build" }] },
        models: { currentModelId: "m", availableModels: [{ modelId: "m", name: "M" }] },
        configOptions: [{ id: "model", name: "Model", currentValue: "m" }],
      },
    });
  });

  test("a load reply with no fields is an empty object, because grok echoes no id", () => {
    // The id the caller sent is the id that was loaded, so an absent one is a
    // valid answer rather than a malformed one.
    expect(decodeLoadSessionResult({})).toEqual({ kind: "decoded", value: {} });
  });

  test("an id that is not a string is dropped, so it cannot be read as a session", () => {
    expect(decodeLoadSessionResult({ sessionId: 7 })).toEqual({ kind: "decoded", value: {} });
  });

  test("a reply that is not an object is refused", () => {
    expect(decodeLoadSessionResult(7).kind).toBe("rejected");
  });
});
