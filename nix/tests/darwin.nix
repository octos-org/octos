{
  pkgs,
  lib,
  nix-darwin,
  octosModule,
}:

let
  eval = nix-darwin.lib.darwinSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      octosModule
      {
        programs.octos = {
          enable = true;
          # enableExtraPackages = true;
          enableAppSkills = true;
          channels = [
            "telegram"
            "discord"
          ];
          service = {
            enable = true;
            port = 50080;
            dataDir = "/var/lib/octos-test";
            authToken = "test-token";
          };
        };

        # Minimal required nix-darwin config
        system.stateVersion = 4;
      }
    ];
  };

  serviceName = "org.octos.serve";
in

pkgs.runCommand "octos-darwin-test" { } ''
  echo "Checking darwin module evaluation..."
  # Check if the launchd agent is defined
  grep -q "${serviceName}" <<EOF
  ${lib.generators.toPlist {
    escape = true;
  } eval.config.launchd.daemons."${serviceName}".serviceConfig}
  EOF

  echo "Darwin module evaluated successfully."
  touch $out
''
