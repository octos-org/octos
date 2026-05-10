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
    mkOption
    mkPackageOption
    types
    ;

  inherit (types)
    enum
    int
    listOf
    str
    submodule
    ;

  cfg = config.programs.octos;
in

{

  options = {
    programs.octos = {
      enable = mkEnableOption "octos CLI";

      package = mkPackageOption selfPackages "octos-minimal" { } // {
        apply = pkg: if cfg.features != [ ] then pkg.override { inherit (cfg) features; } else pkg;
        description = ''
          The octos package to use.

          Features will be automatically applied based on the `features` option 
          and whether skills or service are enabled.
        '';
      };

      enableExtraPackages = mkEnableOption "extra runtime dependencies for octos" // {
        description = ''
          Whether to install optional runtime dependencies (chromium, nodejs, ffmpeg, libreoffice, poppler-utils).

          See <https://github.com/octos-org/octos/blob/main/book/src/installation.md#optional-dependencies>
        '';
      };

      features = mkOption {
        type = listOf (enum [
          "api"
          "telegram"
          "discord"
          "slack"
          "whatsapp"
          "feishu"
          "email"
          "twilio"
          "wecom"
        ]);
        default = [ ];
        example = [
          "telegram"
          "discord"
        ];
        description = "Cargo features to enable";
        apply =
          features:
          if !lib.elem "api" features && (cfg.skills.enable || cfg.service.enable) then
            features ++ [ "api" ]
          else
            features;
      };

      skills = mkOption {
        type = submodule {
          options = {
            enable = mkEnableOption "Install app-skills (news, weather, etc.). Requires features containing api.";
            package = mkPackageOption selfPackages "octos-app-skills" { };
          };
        };
        default = { };
        description = "App Skills configuration";
      };

      service = mkOption {
        type = submodule {
          options = {
            enable = mkEnableOption "octos service (dashboard + gateway)";
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

  config = lib.mkIf cfg.enable {

    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optional cfg.skills.enable cfg.skills.package
    ++ lib.optionals cfg.enableExtraPackages [
      pkgs.chromium
      pkgs.ffmpeg
      pkgs.libreoffice
      pkgs.nodejs
      pkgs.poppler_utils
    ];

  };

}
