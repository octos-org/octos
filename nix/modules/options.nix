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
      enableDashboard = mkEnableOption "octos web dashboard";
      enableExtraPackages = mkEnableOption "Auto-install optional runtime deps based on features";

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
          "browser"
        ]);
        default = [ ];
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
            enable = mkEnableOption "octos serve service";
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
            user = mkOption {
              type = str;
              default = "octos";
              description = "User to run octos serve as";
            };
          };
        };
        default = { };
        description = "octos serve daemon configuration";
      };

      remote = mkOption {
        type = submodule {
          options = {
            enable = mkEnableOption "frpc tunnel for public access";
            domain = mkOption {
              type = str;
              default = "";
              description = "Base domain for tunnel (e.g. alice.octos-cloud.org)";
            };
            frpsServer = mkOption {
              type = str;
              default = "";
              description = "frps server address";
            };
            frpsToken = mkOption {
              type = str;
              default = "";
              description = "frps authentication token";
            };
            sshPort = mkOption {
              type = int;
              default = 6001;
              description = "SSH tunnel remote port";
            };
            tenantName = mkOption {
              type = str;
              default = "";
              description = "Tenant subdomain (e.g. 'alice' for alice.octos-cloud.org)";
            };
          };
        };
        default = { };
        description = "frpc tunnel configuration for remote access — maps to services.frp.instances.octos";
      };
    };
  };
}
