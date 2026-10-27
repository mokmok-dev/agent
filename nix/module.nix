# The NixOS module. Declarative configuration for one daemon and its store.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.mokmok-agent;
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;

  # The daemon reads a TOML file, so the module writes one into the store. The
  # key is never part of it: it is named, and systemd hands the value over.
  settings = {
    base_url = cfg.baseUrl;
    inherit (cfg) directive;
    directive_version = cfg.directiveVersion;
    max_output_tokens = cfg.maxOutputTokens;
    inherit (cfg) model;
    state_dir = "/var/lib/${cfg.stateDirectory}";
    timeout_seconds = cfg.timeoutSeconds;
    tools = map (tool: {
      inherit (tool)
        argv
        class
        name
        risk
        schema
        ;
      max_output_bytes = tool.maxOutputBytes;
      permissions = {
        inherit (tool) env network write;
        file_bytes = tool.fileBytes;
        memory_bytes = tool.memoryBytes;
      };
      timeout_ms = tool.timeoutMs;
    }) cfg.tools;
  }
  // lib.optionalAttrs (cfg.apiKeyFile != null) {
    api_key_file = "/run/credentials/mokmok-agent.service/api_key";
  }
  // lib.optionalAttrs (cfg.apiKeyEnv != null) {
    api_key_env = cfg.apiKeyEnv;
  };

  configFile = (pkgs.formats.toml { }).generate "mokmok-agent.toml" settings;
in
{
  options.services.mokmok-agent = {
    apiKeyEnv = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        The name of an environment variable holding the key. Use this with
        {option}`environmentFile` when the secret already lives in a file that
        systemd can read.
      '';
    };

    apiKeyFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        A file holding the API key. It is handed to the service through
        systemd's credential directory, so the daemon can read it and nothing
        else on the machine can.
      '';
    };

    baseUrl = mkOption {
      type = types.str;
      example = "https://api.deepseek.com/v1";
      description = ''
        The provider's base URL. The daemon appends `/chat/completions`, so this
        is the part before it.
      '';
    };

    directive = mkOption {
      type = types.str;
      description = ''
        The standing instructions. They are the first bytes of every request, so
        they must not contain a clock reading, a host name, or anything else that
        changes between turns.
      '';
    };

    directiveVersion = mkOption {
      type = types.str;
      default = "1";
      description = ''
        A label recorded beside each request. It is not part of the prompt, so
        bumping it alone does not cost a cache.
      '';
    };

    enable = mkEnableOption "the mokmok agent daemon";

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "A file of environment variables for the service.";
    };

    group = mkOption {
      type = types.str;
      default = "mokmok-agent";
      description = "The group the daemon runs as.";
    };

    maxOutputTokens = mkOption {
      type = types.ints.positive;
      default = 4096;
      description = "The cap on one response.";
    };

    memoryMax = mkOption {
      type = types.str;
      default = "4G";
      description = "The memory ceiling for the service.";
    };
    model = mkOption {
      type = types.str;
      example = "deepseek-chat";
      description = "The model id sent to the provider.";
    };

    agentctlPackage = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.agentctl;
      defaultText = lib.literalExpression "self.packages.\${system}.agentctl";
      description = ''
        The operator's command line, installed on the host. A service nobody can
        send a command to is a service that only runs what it was built with.
      '';
    };

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.agentd;
      defaultText = lib.literalExpression "self.packages.\${system}.agentd";
      description = "The daemon to run.";
    };

    stateDirectory = mkOption {
      type = types.str;
      default = "mokmok-agent";
      description = ''
        The name systemd gives the state directory, created under `/var/lib`
        and owned by the service. The store lives in it.
      '';
    };

    timeoutSeconds = mkOption {
      type = types.ints.positive;
      default = 120;
      description = "The deadline for one model call.";
    };

    sandbox = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Whether a tool's process runs inside the bounds its manifest declares. The
        bounds are syscalls the kernel holds, so this costs nothing but a fork. A
        host whose kernel cannot hold them refuses to start, and this flag is for a
        host that must run tools anyway: it is the only way to say so.
      '';
    };

    tools = mkOption {
      type = types.listOf (
        types.submodule {
          options = {
            argv = mkOption {
              type = types.listOf types.str;
              description = ''
                The program and its arguments. The model's arguments arrive on
                the tool's standard input, so there is no shell and no template.
              '';
            };
            class = mkOption {
              type = types.enum [
                "idempotent"
                "non-idempotent"
              ];
              default = "idempotent";
              description = ''
                Whether running it twice is the same as running it once. A tool that
                is not idempotent is never re-run after a crash; the log records
                that the effect may have happened instead.
              '';
            };
            env = mkOption {
              type = types.listOf types.str;
              description = ''
                The environment variable names the tool's process may see. It sees
                no others, so a tool that needs PATH or HOME has to say so. There is
                no default: an empty list is a real answer and means the process runs
                with an empty environment, but it is still the operator's answer.
              '';
            };
            fileBytes = mkOption {
              type = types.ints.positive;
              default = 8 * 1024 * 1024;
              description = ''
                The largest file the tool may write. The bound is the kernel's, and a
                tool that passes it is stopped rather than allowed to fill the disk.
              '';
            };
            memoryBytes = mkOption {
              type = types.ints.positive;
              default = 256 * 1024 * 1024;
              description = ''
                The address space the tool's process may use. A tool that asks for
                more fails inside itself; it cannot borrow the daemon's headroom.
              '';
            };
            network = mkOption {
              type = types.enum [
                "any"
                "none"
              ];
              description = ''
                Whether the process may reach the network. The kernel gives a
                namespace with none or shares the host's; there is no allowlist,
                because a hostname list would be a promise this build cannot keep.
                There is no default: reaching the network is the operator's
                statement, not this module's guess.
              '';
            };
            write = mkOption {
              type = types.listOf types.str;
              default = [ ];
              description = ''
                The directories the tool may write to. Every other path is mounted
                read-only, including its working directory, so a tool that produces
                something has to say where. Absolute, because the launcher resolves
                a relative path against its own working directory and not the
                tool's.
              '';
            };
            maxOutputBytes = mkOption {
              type = types.ints.positive;
              default = 65536;
              description = "The most of its output one event may carry.";
            };
            name = mkOption {
              type = types.str;
              description = "The tool's name, unique within the set.";
            };
            risk = mkOption {
              type = types.enum [
                "destructive"
                "network"
                "read"
                "write"
              ];
              description = ''
                What it may touch. Network is approved once per branch, destructive
                every time, and read and write inside the declared paths run without
                asking. There is no default: what a tool may touch is the operator's
                statement, not this module's guess.
              '';
            };
            schema = mkOption {
              type = types.str;
              description = "The tool's schema, as the model will read it.";
            };
            timeoutMs = mkOption {
              type = types.ints.positive;
              default = 30000;
              description = "How long one call may run before it is stopped.";
            };
          };
        }
      );
      default = [ ];
      description = "The tools in force. They are sent in name order.";
    };

    user = mkOption {
      type = types.str;
      default = "mokmok-agent";
      description = "The user the daemon runs as.";
    };

    watchdogSec = mkOption {
      type = types.ints.positive;
      default = 30;
      description = ''
        How long the loop may go without a heartbeat before the service is
        restarted. The ping is sent after each iteration completes.
      '';
    };

  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.apiKeyFile != null && cfg.apiKeyEnv != null);
        message = "services.mokmok-agent: set apiKeyFile or apiKeyEnv, not both.";
      }
    ];

    environment.systemPackages = [ cfg.agentctlPackage ];

    users.groups.${cfg.group} = { };
    users.users.${cfg.user} = {
      isSystemUser = true;
      inherit (cfg) group;
      home = "/var/lib/${cfg.stateDirectory}";
      description = "the mokmok agent";
    };

    systemd.services.mokmok-agent = {
      description = "mokmok agent";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      # The rate limit is off because the limiter that matters is the one inside
      # the agent, persisted in the log as schedule.armed. This is a [Unit] key:
      # systemd ignores it in [Service] and says so in the journal, which is how
      # this was found on a machine that had been running it for two minutes.
      startLimitIntervalSec = 0;

      serviceConfig = {
        Type = "notify";
        WatchdogSec = "${toString cfg.watchdogSec}s";
        ExecStart = "${cfg.package}/bin/agentd --config ${configFile}";

        # A crash is a restart, not an outage. There is no rate limit here: see
        # startLimitIntervalSec above.
        Restart = "always";
        RestartSec = 2;
        TimeoutStopSec = 45;

        User = cfg.user;
        Group = cfg.group;
        StateDirectory = cfg.stateDirectory;
        StateDirectoryMode = "0700";

        MemoryMax = cfg.memoryMax;
        TasksMax = 512;

        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectClock = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RemoveIPC = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
      }
      // lib.optionalAttrs (cfg.apiKeyFile != null) {
        LoadCredential = "api_key:${cfg.apiKeyFile}";
      }
      // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      };
    };

    # A directory a tool may write has to be there before the sandbox mounts it,
    # and the daemon does not create directories it was only given permission to
    # use. The deployment creates them.
    systemd.tmpfiles.rules = lib.concatMap (
      tool: map (path: "d ${path} 0750 ${cfg.user} ${cfg.group} - -") tool.write
    ) cfg.tools;
  };
}
