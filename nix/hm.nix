{
  self,
}:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.telagent;
  tomlFormat = pkgs.formats.toml { };

  settings = {
    telegram = {
      allowed_user_ids = cfg.telegram.allowedUserIds;
    }
    // lib.optionalAttrs (cfg.telegram.botTokenFile != null) {
      bot_token_file = cfg.telegram.botTokenFile;
    }
    // lib.optionalAttrs (cfg.telegram.proxy != null) {
      proxy = cfg.telegram.proxy;
    };

    opencode = {
      backend = cfg.opencode.backend;
      url = cfg.opencode.url;
      username = cfg.opencode.username;
      events_path = cfg.opencode.eventsPath;
      auto_discover = cfg.opencode.autoDiscover;
    }
    // lib.optionalAttrs (cfg.opencode.passwordFile != null) {
      password_file = cfg.opencode.passwordFile;
    }
    // lib.optionalAttrs (cfg.opencode.serviceFile != null) {
      service_file = cfg.opencode.serviceFile;
    };

    notify = {
      on_idle = cfg.notify.onIdle;
      on_error = cfg.notify.onError;
      on_created = cfg.notify.onCreated;
      permissions = cfg.notify.permissions;
      questions = cfg.notify.questions;
      child_sessions = cfg.notify.childSessions;
      include_last_message = cfg.notify.includeLastMessage;
      max_telegram_chars = cfg.notify.maxTelegramChars;
    };

    paste = {
      enabled = cfg.paste.enabled;
      url = cfg.paste.url;
      threshold_chars = cfg.paste.thresholdChars;
    };

    security = {
      pairing_ttl_secs = cfg.security.pairingTtlSecs;
      require_bind = cfg.security.requireBind;
    };
  }
  // lib.optionalAttrs (cfg.instanceId != null) {
    instance_id = cfg.instanceId;
  }
  // lib.optionalAttrs (cfg.storage.path != null) {
    storage.path = cfg.storage.path;
  };

  configFile = tomlFormat.generate "telagent.toml" settings;
in
{
  options.services.telagent = {
    enable = lib.mkEnableOption "telagent Telegram bridge for opencode";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "self.packages.\${system}.default";
      description = "The telagent package to use.";
    };

    instanceId = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Stable instance identifier. Generated automatically when null.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "telagent=info";
      description = "RUST_LOG filter for the telagent service.";
    };

    telegram = {
      botTokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Path to a file containing the Telegram bot token.";
      };

      allowedUserIds = lib.mkOption {
        type = lib.types.listOf lib.types.int;
        default = [ ];
        description = "Telegram user IDs allowed to use the bot. Empty means anyone with a pairing code.";
      };

      proxy = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Proxy URL for Telegram API connections, exported as TELEGRAM_PROXY.";
      };
    };

    opencode = {
      backend = lib.mkOption {
        type = lib.types.enum [
          "v1"
          "v1-http"
          "v2"
          "v2-http"
        ];
        default = "v1";
        description = "opencode API backend.";
      };

      url = lib.mkOption {
        type = lib.types.str;
        default = "http://127.0.0.1:4096";
        description = "Base URL of the local opencode server.";
      };

      username = lib.mkOption {
        type = lib.types.str;
        default = "opencode";
        description = "Basic auth username for the opencode server.";
      };

      passwordFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Path to a file containing the opencode basic auth password.";
      };

      eventsPath = lib.mkOption {
        type = lib.types.str;
        default = "/global/event";
        description = "opencode SSE endpoint.";
      };

      autoDiscover = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Read the opencode V2 service URL and auth from service.json.";
      };

      serviceFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Path to the opencode V2 service.json. Defaults to ~/.local/state/opencode/service.json.";
      };
    };

    notify = {
      onIdle = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };

      onError = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };

      onCreated = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };

      permissions = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };

      questions = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };

      childSessions = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };

      includeLastMessage = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };

      maxTelegramChars = lib.mkOption {
        type = lib.types.ints.positive;
        default = 3900;
      };
    };

    paste = {
      enabled = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Upload long notifications and /export pastes to cocoon as private pastes.";
      };

      url = lib.mkOption {
        type = lib.types.str;
        default = "https://cocoon.sereph.us";
        description = "Base URL of the cocoon pastebin service.";
      };

      thresholdChars = lib.mkOption {
        type = lib.types.ints.positive;
        default = 12000;
      };
    };

    storage = {
      path = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "redb database path. Defaults to ~/.local/state/telagent/state.redb.";
      };
    };

    security = {
      pairingTtlSecs = lib.mkOption {
        type = lib.types.ints.positive;
        default = 600;
      };

      requireBind = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."telagent/config.toml".source = configFile;

    systemd.user.services.telagent = {
      Unit = {
        Description = "telagent Telegram bridge for opencode";
        After = [ "network-online.target" ];
        Wants = [ "network-online.target" ];
      };

      Service = {
        ExecStart = "${cfg.package}/bin/telagent daemon --config ${configFile}";
        Restart = "on-failure";
        RestartSec = 5;
        Environment = [ "RUST_LOG=${cfg.logLevel}" ]
          ++ lib.optional (cfg.telegram.proxy != null) "TELEGRAM_PROXY=${cfg.telegram.proxy}";
        NoNewPrivileges = true;
        PrivateTmp = true;
        StateDirectory = "telagent";
      };

      Install.WantedBy = [ "default.target" ];
    };
  };
}
