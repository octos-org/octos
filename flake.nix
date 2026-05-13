{
  description = "Octos - Agentic OS";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/*";
    nix-darwin = {
      url = "https://flakehub.com/f/nix-darwin/nix-darwin/*";
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
              inherit (self.packages.${system}) octos;
            })
            modulePath
          ];
        };
    in
    {
      nixosModules.default = mkModuleWithPackages ./nix/modules/nixos.nix;
      darwinModules.default = mkModuleWithPackages ./nix/modules/darwin.nix;

      formatter = forEachSupportedSystem ({ pkgs, ... }: pkgs.nixfmt-tree);

      devShells = forEachSupportedSystem (
        { pkgs, ... }:
        {
          default = pkgs.callPackage ./nix/shell.nix { };
        }
      );

      packages = forEachSupportedSystem (
        { pkgs, ... }:
        let
          octos = pkgs.callPackage ./nix/packages/default.nix { };
        in
        {
          inherit octos;
          default = octos;
          octos-minimal = octos;
          octos-full = octos.override {
            enableAllFeatures = true;
            enableAppSkills = true;
          };
        }
      );

      checks = forEachSupportedSystem (
        { pkgs, ... }:
        {
          # 1. Darwin Module Evaluation (Cross-platform)
          darwin-module = pkgs.callPackage ./nix/tests/darwin.nix {
            inherit (inputs) nix-darwin;
            octosModule = self.darwinModules.default;
          };
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # 4. NixOS VM Test (Linux only, full E2E)
          nixos-module-vm = pkgs.callPackage ./nix/tests/nixos.nix {
            octosModule = self.nixosModules.default;
          };
        }
      );
    };
}
