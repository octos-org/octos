{
  description = "Octos - Agentic OS";

  inputs = {
    nixpkgs.url = "nixpkgs/nixos-unstable";
    nix-darwin = {
      url = "github:nix-darwin/nix-darwin";
      inputs.nixpkgs.follows = "nixpkgs";
    };
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

      mkModuleWithPackages =
        modulePath:
        { pkgs, lib, ... }:
        let
          inherit (pkgs.stdenv.hostPlatform) system;
        in
        {
          imports = [
            (lib.modules.importApply ./nix/modules/options.nix {
              inherit (self.packages.${system}) octos-minimal octos-app-skills;
            })
            modulePath
          ];
        };
    in
    {
      nixosModules.default = mkModuleWithPackages ./nix/modules/nixos.nix;
      darwinModules.default = mkModuleWithPackages ./nix/modules/darwin.nix;

      devShells = forEachSupportedSystem (
        { pkgs, ... }:
        {
          default = pkgs.callPackage ./nix/shell.nix { };
        }
      );

      packages = forEachSupportedSystem (
        { pkgs, system }:
        {
          octos-minimal = pkgs.callPackage ./nix/packages/default.nix { };
          octos-app-skills = pkgs.callPackage ./nix/packages/app-skills.nix { };

          default = self.packages.${system}.octos-minimal;

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
