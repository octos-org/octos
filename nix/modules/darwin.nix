{ config, lib, ... }:
let
  cfg = config.programs.octos;
  serviceCfg = cfg.service;

  serviceName = "org.octos.serve";
in
{
  config = lib.mkIf cfg.enable {

    system.activationScripts.octos-datadir = lib.mkIf serviceCfg.enable ''
      mkdir -p ${lib.escapeShellArg serviceCfg.dataDir}
      chmod 770 ${lib.escapeShellArg serviceCfg.dataDir}
      chown root:wheel ${lib.escapeShellArg serviceCfg.dataDir}
    '';

    launchd.daemons.${serviceName} = lib.mkIf serviceCfg.enable {
      script = ''
        exec ${cfg.finalPackage}/bin/octos serve \
          --port ${toString serviceCfg.port} \
          --host ${lib.escapeShellArg serviceCfg.host}
      '';

      environment = {
        OCTOS_DATA_DIR = serviceCfg.dataDir;
        OCTOS_AUTH_TOKEN = serviceCfg.authToken;
      };

      serviceConfig = {
        Label = serviceName;
        KeepAlive = true;
        RunAtLoad = true;
        WorkingDirectory = serviceCfg.dataDir;
        StandardOutPath = "/var/log/octos.out.log";
        StandardErrorPath = "/var/log/octos.err.log";
      };
    };

  };
}
