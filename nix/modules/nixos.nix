{
  config,
  lib,
  ...
}:

let
  cfg = config.programs.octos;

  finalFeatures =
    if ((cfg.enableDashboard || cfg.skills.enable) && !builtins.elem "api" cfg.features) then
      cfg.features ++ [ "api" ]
    else
      cfg.features;

  finalPackage =
    if finalFeatures != [ ] then cfg.package.override { features = finalFeatures; } else cfg.package;
in

{

  config = lib.mkIf cfg.enable {

    environment.systemPackages = [ finalPackage ] ++ lib.optional cfg.skills.enable cfg.skills.package;
  };

}
