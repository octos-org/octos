{
  description = "Octos - Agentic OS";

  inputs = {
    nixpkgs.url = "nixpkgs/nixos-unstable";
  };

  outputs =
    { self, ... }@inputs:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forEachSupportedSystem =
        f:
        inputs.nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            inherit system;
            pkgs = import inputs.nixpkgs { inherit system; };
          }
        );
    in
    {
      nixosModules.default = import ./nix/modules/nixos.nix;
      darwinModules.default = import ./nix/modules/darwin.nix;
      homeModules.default = import ./nix/modules/home.nix;

      devShells = forEachSupportedSystem (
        { pkgs, ... }:
        {
          default = pkgs.callPackage ./nix/shell.nix { };
        }
      );

      packages = forEachSupportedSystem (
        { pkgs, system }:
        {
          default = self.packages.${system}.octos-minimal;
          octos-app-skills = pkgs.callPackage ./nix/packages/app-skills.nix { };
          octos-minimal = pkgs.callPackage ./nix/packages/default.nix { };
          octos-full = pkgs.buildEnv {
            name = "octos-full";
            paths = [
              self.packages.${system}.octos-app-skills

              (self.packages.${system}.octos-minimal.override {
                features = [
                  "api"
                  "telegram"
                  "discord"
                  "slack"
                  "whatsapp"
                  "feishu"
                  "email"
                  "twilio"
                  "wecom"
                ];
              })
            ];
          };
        }
      );

    };
}
