{
  lib,
  stdenv,
  rustPlatform,
  pkg-config,
  features ? [ ],
}:
let
  rustTarget = stdenv.hostPlatform.rust.rustcTarget;
  cargoFeaturesString = builtins.concatStringsSep "," features;
in
rustPlatform.buildRustPackage {
  version = "0.0.1";
  pname = "octos";
  src = lib.cleanSource ../../.;
  cargoLock.lockFile = ../../Cargo.lock;

  doCheck = false;

  nativeBuildInputs = [ pkg-config ];

  cargoBuildFlags = [
    "-p"
    "octos-cli"
  ]
  ++ lib.optionals (features != [ ]) [
    "--features"
    cargoFeaturesString
  ];

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
