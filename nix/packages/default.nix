{
  lib,
  stdenv,
  callPackage,
  rustPlatform,
  pkg-config,
  features ? [ ],
}:
rustPlatform.buildRustPackage {
  version = "0.1.1";
  pname = "octos-cli";
  src = lib.cleanSource ../../.;
  cargoLock.lockFile = ../../Cargo.lock;

  doCheck = false;

  nativeBuildInputs = [ pkg-config ];

  cargoBuildFlags =
    let
      cargoFeaturesString = builtins.concatStringsSep "," features;
    in
    [
      "-p"
      "octos-cli"
    ]
    ++ lib.optionals (features != [ ]) [
      "--features"
      cargoFeaturesString
    ];

  preBuild =
    let
      dashboardPkg = callPackage ./dashboard.nix { };
    in
    lib.optionalString (builtins.elem "api" features) ''
      mkdir -p crates/octos-cli/static/admin
      cp -r ${dashboardPkg}/admin/* crates/octos-cli/static/admin/
    '';

  installPhase =
    let
      rustTarget = stdenv.hostPlatform.rust.rustcTarget;
    in
    ''
      runHook preInstall
      mkdir -p $out/bin
      install -Dm755 ./target/${rustTarget}/release/octos $out/bin/octos
      runHook postInstall
    '';

  meta = with lib; {
    description = "CLI interface for octos - Agentic OS";
    homepage = "https://github.com/octos-org/octos";
    license = licenses.asl20;
    maintainers = [ ];
    platforms = platforms.linux ++ platforms.darwin;
  };
}
