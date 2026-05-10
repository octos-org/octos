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

    launchd.daemons."org.octos.gateway" = lib.mkIf serviceCfg.enable {
      command = "${finalPackage}/bin/octos";
      arguments = [
        "serve"
        "--port"
        "${toString serviceCfg.port}"
        "--host"
        "${serviceCfg.host}"
        "--auth-token"
        "${serviceCfg.authToken}"
      ];
      environment = {
        OCTOS_DATA_DIR = serviceCfg.dataDir;
      };
      serviceConfig = {
        RunAtLoad = true;
        KeepAlive = true;
        WorkingDirectory = serviceCfg.dataDir;
      };
    };

    launchd.activationScripts.octos-datadir = lib.mkIf serviceCfg.enable ''
      mkdir -p ${lib.escapeShellArg serviceCfg.dataDir}
      chmod 770 ${lib.escapeShellArg serviceCfg.dataDir}
    '';

  };
}

