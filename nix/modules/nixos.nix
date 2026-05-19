{ config, lib, ... }:
let
  cfg = config.programs.octos;
  serviceCfg = cfg.service;
in
{
  config = lib.mkIf cfg.enable {

    systemd.tmpfiles.rules = lib.mkIf serviceCfg.enable [
      "d ${serviceCfg.dataDir} 0770 root root -"
    ];

    systemd.services.octos-serve = lib.mkIf serviceCfg.enable {
      script = ''
        exec ${cfg.finalPackage}/bin/octos serve \
          --port ${toString serviceCfg.port} \
          --host ${lib.escapeShellArg serviceCfg.host} \
          --data-dir ${lib.escapeShellArg serviceCfg.dataDir} \
          --auth-token ${lib.escapeShellArg serviceCfg.authToken}
      '';

      environment = {
        OCTOS_DATA_DIR = serviceCfg.dataDir;
        OCTOS_AUTH_TOKEN = serviceCfg.authToken;
      };

      serviceConfig = {
        RestartSec = 5;
        Restart = "on-failure";
        WorkingDirectory = serviceCfg.dataDir;
        StandardOutput = "journal";
        StandardError = "journal";
      };

      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      description = "octos serve";
    };

  };
}
