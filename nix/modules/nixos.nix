{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.octos;

  finalFeatures =
    if !lib.elem "api" cfg.features && (cfg.enableDashboard || cfg.skills.enable) then
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
    ++ lib.optional cfg.enableExtraPackages [
      pkgs.chromium
      pkgs.ffmpeg
      pkgs.libreoffice
      pkgs.nodejs
      pkgs.poppler_utils
    ];

  };
}
