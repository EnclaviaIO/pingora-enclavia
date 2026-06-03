# NixOS module for pingora-enclavia.
#
# Enable with:
#
#   imports = [ inputs.pingora-enclavia.nixosModules.default ];
#   services.pingora-enclavia.enable = true;
#
# The proxy listens on a plain HTTP port. In production deploy nginx
# (or another front-end) in front of it for TLS, the WebSocket upgrade
# dance, and routing. A typical nginx snippet:
#
#   map $http_upgrade $connection_upgrade {
#     default upgrade;
#     ''      close;
#   }
#
#   location /proxy/ {
#     rewrite ^/proxy/(.*) /$1 break;
#     proxy_pass http://127.0.0.1:6188;
#     proxy_set_header Host $host;
#     proxy_http_version 1.1;
#     proxy_set_header Upgrade $http_upgrade;
#     proxy_set_header Connection $connection_upgrade;
#     proxy_read_timeout 3600s;
#     proxy_send_timeout 3600s;
#   }
#
# pingora-enclavia is intentionally unopinionated about its front-end:
# this module installs the service and lets the operator wire nginx
# (or anything else) themselves.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.pingora-enclavia;
in
{
  options.services.pingora-enclavia = {
    enable = lib.mkEnableOption "the pingora-enclavia attested HTTP proxy";

    package = lib.mkOption {
      type = lib.types.package;
      description = "pingora-enclavia package to run.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "pingora-enclavia";
      description = "System user the service runs as.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "pingora-enclavia";
      description = ''
        System group the service runs as. Other services that need
        write access to `configDir` (for example, a controller that
        writes per-target JSON files) should add their own user to
        this group via `users.users.<name>.extraGroups`.
      '';
    };

    configDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/pingora-enclavia/targets";
      description = ''
        Directory the proxy watches for per-enclave target JSON files
        (one `<uuid>.json` per running enclave). The proxy reads
        read-only and re-loads on inotify events. A tmpfiles rule
        creates this directory mode `2770` owned by `${cfg.user}:${cfg.group}`,
        so any user in `${cfg.group}` can write target files.
      '';
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:6188";
      description = ''
        Address the HTTP listener binds on. Default `127.0.0.1:6188`
        assumes a front-end (nginx, Caddy, etc.) handles TLS and
        forwards plaintext to the proxy.
      '';
    };

    tunnelTimeoutSecs = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 10;
      description = ''
        Time budget in seconds for the full TLS+WSS+Noise+attestation
        handshake against the upstream enclave.
      '';
    };

    requestTimeoutSecs = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 30;
      description = ''
        Per-request response timeout in seconds (head + body) applied
        as the upstream read/write timeout.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = ''
        `RUST_LOG` filter passed to the service. Use the standard
        `tracing-subscriber` syntax, e.g.
        `info,pingora_enclavia=debug`.
      '';
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra command-line arguments appended to the binary invocation.";
    };

    targetsGroup = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
      default = cfg.group;
      description = ''
        Read-only helper: name of the group that owns `configDir`.
        Downstream modules that need write access to the targets
        directory should add their service user to this group.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
    };
    users.groups.${cfg.group} = { };

    systemd.tmpfiles.rules = [
      "d ${cfg.configDir} 2770 ${cfg.user} ${cfg.group} -"
    ];

    systemd.services.pingora-enclavia = {
      description = "pingora-enclavia (attested HTTP proxy for Nitro enclaves)";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        PROXY_TARGETS_DIR = toString cfg.configDir;
        LISTEN = cfg.listen;
        TUNNEL_TIMEOUT_SECS = toString cfg.tunnelTimeoutSecs;
        REQUEST_TIMEOUT_SECS = toString cfg.requestTimeoutSecs;
        RUST_LOG = cfg.logLevel;
      };

      serviceConfig = {
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/pingora-enclavia"
        ] ++ cfg.extraArgs);
        User = cfg.user;
        Group = cfg.group;
        Restart = "on-failure";
        RestartSec = "5s";

        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        PrivateTmp = true;
        RestrictNamespaces = true;
        LockPersonality = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
      };
    };
  };
}
