{
  config,
  lib,
  ...
}:

let
  cfg = config.programs.octos;
in

{

  config = lib.mkIf cfg.enable {

    environment.systemPackages = [ cfg.package ] ++ lib.optional cfg.skills.enable cfg.skills.package;
  };

}
