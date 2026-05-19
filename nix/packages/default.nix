{
  pkgs,
  lib,
  enableAllFeatures ? false,
  enableAppSkills ? false,
  features ? null,
}:

let
  octos-cli = pkgs.callPackage ./cli.nix { features = actualFeatures; };
  octos-app-skills = pkgs.callPackage ./app-skills.nix { };

  actualFeatures =
    if features != null then
      features
    else if enableAllFeatures then
      octos-cli.supportedFeatures
    else
      [ ];
in

pkgs.buildEnv {
  name = "octos";
  paths = [ octos-cli ] ++ lib.optionals enableAppSkills [ octos-app-skills ];

  passthru = {
    inherit octos-cli octos-app-skills;
  };

  meta = with lib; {
    description = "Octos - Agentic OS";
    homepage = "https://github.com/octos-org/octos";
    license = licenses.asl20;
    maintainers = [ ];
    platforms = platforms.linux ++ platforms.darwin;
  };
}
