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
      inherit (tool) name schema;
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

    tools = mkOption {
      type = types.listOf (
        types.submodule {
          options = {
            name = mkOption {
              type = types.str;
              description = "The tool's name, unique within the set.";
            };
            schema = mkOption {
              type = types.str;
              description = "The tool's schema, as the model will read it.";
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
  };
}
