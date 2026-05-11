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
              inherit (self.packages.${system}) octos;
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

    };
}
