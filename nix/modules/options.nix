selfPackages:

{
  lib,
  config,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    mkPackageOption
    optional
    optionals
    types
    ;

  inherit (types)
    bool
    enum
    int
    listOf
    nullOr
    package
    str
    submodule
    ;

  allChannels = selfPackages.octos.octos-cli.supportedChannels;

  cfg = config.programs.octos;
in

{
  options = {
    programs.octos = {
      enable = mkEnableOption "octos CLI";

      package = mkPackageOption selfPackages "octos" { };

      finalPackage = mkOption {
        type = package;
        readOnly = true;
        description = "The final octos package after applying module-level overrides.";
        default =
          let
            # Determine if any feature-related options are explicitly set in the module.
            # Using null defaults allows us to detect "no intent to override".
            featuresRequested = cfg.channels != null || cfg.enableAllChannels || cfg.service.enable;
            skillsRequested = cfg.enableAppSkills != null;

            # Construct the override arguments only if requested.
            overrideArgs =
              (lib.optionalAttrs featuresRequested {
                # sort + dedup inside @nix/packages/cli.nix:36
                features =
                  (if cfg.channels == null then [ ] else cfg.channels)
                  ++ optionals cfg.enableAllChannels allChannels
                  ++ optional (
                    cfg.service.enable || cfg.enableAllChannels || (cfg.channels != null && cfg.channels != [ ])
                  ) "api";
              })
              // (lib.optionalAttrs skillsRequested {
                inherit (cfg) enableAppSkills;
              });
          in
          # Transparent Override Pattern:
          # If no module-level customizations are requested, return the package as-is.
          # This ensures that pre-configured packages (like octos-full) remain bit-identical
          # and reuse existing build caches.
          if overrideArgs == { } then cfg.package else cfg.package.override overrideArgs;
      };

      enableAllChannels = mkEnableOption "all channels (telegram,discord,slack,whatsapp,feishu,email,twilio,wecom)";

      enableAppSkills = mkOption {
        type = nullOr bool;
        default = null;
        description = "Whether to enable app-skills. If null, preserves the package's default.";
      };

      enableExtraPackages = mkEnableOption "extra runtime dependencies for octos" // {
        description = ''
          Whether to install optional runtime dependencies (chromium, nodejs, ffmpeg, libreoffice, poppler-utils).

          See <https://github.com/octos-org/octos/blob/main/book/src/installation.md#optional-dependencies>
        '';
      };

      channels = mkOption {
        type = nullOr (listOf (enum allChannels));
        default = null;
        example = [
          "telegram"
          "discord"
        ];
        description = ''
          Communication channels to enable. If null, preserves the package's default features.

          See <https://github.com/octos-org/octos/blob/main/book/src/channels.md>
        '';
      };

      service = mkOption {
        type = submodule {
          options = {
            enable = mkEnableOption "octos serve (dashboard + gateway)";
            port = mkOption {
              type = int;
              default = 8080;
              description = "Port to listen on";
            };
            host = mkOption {
              type = str;
              default = "127.0.0.1";
              description = "Host to bind to";
            };
            dataDir = mkOption {
              type = str;
              default = "/var/lib/octos";
              description = "octos data directory";
            };
            authToken = mkOption {
              type = str;
              description = "Auth token for dashboard access (required)";
            };
          };
        };
        default = { };
        description = "octos serve daemon configuration";
      };
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [
      cfg.finalPackage
    ]
    ++ optionals cfg.enableExtraPackages [
      pkgs.chromium
      pkgs.ffmpeg
      pkgs.libreoffice
      pkgs.nodejs
      pkgs.poppler-utils
    ];
  };
}
