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
  inherit (lib)
    cleanSource
    concatStringsSep
    elem
    lessThan
    optionalString
    optionals
    sort
    unique
    ;

  supportedChannels = [
    "discord"
    "email"
    "feishu"
    "slack"
    "telegram"
    "twilio"
    "wecom"
    "whatsapp"
  ];
  supportedFeatures = supportedChannels ++ [ "api" ];

  # sort + dedup ensures same set always produces same derivation hash
  cargoFeatures = unique (sort lessThan features);

  cargoFeaturesString = concatStringsSep "," cargoFeatures;
  dashboardPkg = callPackage ./admin-dashboard.nix { };
  rustTarget = stdenv.hostPlatform.rust.rustcTarget;
in

rustPlatform.buildRustPackage {
  pname = "octos-cli";
  version = "0.1.1";
  src = cleanSource ../../.;

  cargoLock.lockFile = ../../Cargo.lock;

  doCheck = false;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  cargoBuildFlags = [
    "-p"
    "octos-cli"
  ]
  ++ optionals (cargoFeatures != [ ]) [
    "--features"
    cargoFeaturesString
  ];

  preBuild = optionalString (elem "api" cargoFeatures) ''
    mkdir -p crates/octos-cli/static/admin
    cp -r ${dashboardPkg}/admin/* crates/octos-cli/static/admin/
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    install -Dm755 ./target/${rustTarget}/release/octos $out/bin/octos
    runHook postInstall
  '';

  passthru = {
    inherit supportedChannels supportedFeatures;
  };
}
