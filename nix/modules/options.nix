selfPackages:

{ lib, ... }:

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
in

{
  options = {
    programs.octos = {
      enable = mkEnableOption "octos CLI";
      package = mkPackageOption selfPackages "octos-minimal" { };
      enableExtraPackages = mkEnableOption ''
        Install optional runtime dependencies (chromium, nodejs, ffmpeg, libreoffice, poppler-utils).

        See https://github.com/octos-org/octos/blob/main/book/src/installation.md#optional-dependencies
      '';

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
}
