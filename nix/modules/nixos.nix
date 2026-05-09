{
  config,
  lib,
  pkgs,
  ...

}:

let
  cfg = config.programs.octos;
  serviceCfg = cfg.service;

  finalFeatures =
    if !lib.elem "api" cfg.features && (cfg.skills.enable || serviceCfg.enable) then
      cfg.features ++ [ "api" ]
    else
      cfg.features;

  finalPackage =
    if finalFeatures != [ ] then cfg.package.override { features = finalFeatures; } else cfg.package;
in

{
  config = lib.mkIf cfg.enable {

    environment.systemPackages = [
      finalPackage
    ]
    ++ lib.optional cfg.skills.enable cfg.skills.package
    ++ lib.optionals cfg.enableExtraPackages [
      pkgs.chromium
      pkgs.ffmpeg
      pkgs.libreoffice
      pkgs.nodejs
      pkgs.poppler_utils
    ];

    systemd.tmpfiles.rules = lib.mkIf serviceCfg.enable [
      "d ${cfg.service.dataDir} 0770 root root -"
    ];

    systemd.services.octos-gateway = lib.mkIf serviceCfg.enable {
      description = "octos gateway service";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        RestartSec = 5;
        Restart = "on-failure";
        ExecStart = "${finalPackage}/bin/octos serve --port ${toString serviceCfg.port} --host ${serviceCfg.host} --auth-token ${serviceCfg.authToken}";
        Environment = [ "OCTOS_DATA_DIR=${serviceCfg.dataDir}" ];
        WorkingDirectory = cfg.service.dataDir;
      };
    };

  };
}
