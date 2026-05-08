{
  lib,
  stdenv,
  callPackage,
  rustPlatform,
  pkg-config,
  openssl,
  features ? [ ],
}:
let
  cargoFeaturesString = builtins.concatStringsSep "," features;
  dashboardPkg = callPackage ./dashboard.nix { };
  rustTarget = stdenv.hostPlatform.rust.rustcTarget;
in
rustPlatform.buildRustPackage {
  pname = "octos-cli";
  version = "0.1.1";
  src = lib.cleanSource ../../.;

  cargoLock.lockFile = ../../Cargo.lock;

  doCheck = false;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  cargoBuildFlags = [
    "-p"
    "octos-cli"
  ]
  ++ lib.optionals (features != [ ]) [
    "--features"
    cargoFeaturesString
  ];

  preBuild = lib.optionalString (builtins.elem "api" features) ''
    mkdir -p crates/octos-cli/static/admin
    cp -r ${dashboardPkg}/admin/* crates/octos-cli/static/admin/
  '';

  installPhase = ''
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
