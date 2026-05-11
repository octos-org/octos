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
    unique
    ;

  inherit (types)
    enum
    int
    listOf
    package
    str
    submodule
    ;

  cfg = config.programs.octos;

  allChannels = [
    "discord"
    "email"
    "feishu"
    "slack"
    "telegram"
    "twilio"
    "wecom"
    "whatsapp"
  ];
in

{

  options = {
    programs.octos = {
      enable = mkEnableOption "octos CLI";

      package = mkPackageOption selfPackages "octos" { };

      finalFeatures = mkOption {
        type = listOf str;
        internal = true;
        visible = false;
        default = unique (
          cfg.channels
          ++ optionals cfg.enableAllChannels allChannels
          ++ optional (cfg.channels != [ ] || cfg.enableAllChannels || cfg.service.enable) "api"
        );
      };

      finalPackage = mkOption {
        type = package;
        internal = true;
        visible = false;
        default = cfg.package.override {
          features = cfg.finalFeatures;
          inherit (cfg) enableAppSkills;
        };
      };

      enableAllChannels = mkEnableOption "all channels (telegram,discord,slack,whatsapp,feishu,email,twilio,wecom)";

      enableAppSkills = mkEnableOption "app-skills (news, weather, etc.)";

      enableExtraPackages = mkEnableOption "extra runtime dependencies for octos" // {
        description = ''
          Whether to install optional runtime dependencies (chromium, nodejs, ffmpeg, libreoffice, poppler-utils).

          See <https://github.com/octos-org/octos/blob/main/book/src/installation.md#optional-dependencies>
        '';
      };

      channels = mkOption {
        type = listOf (enum allChannels);
        default = [ ];
        example = [
          "telegram"
          "discord"
        ];
        description = ''
          Communication channels to enable. Each channel will be compiled into the octos binary as a Cargo feature.

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
      pkgs.poppler_utils
    ];

  };

}
