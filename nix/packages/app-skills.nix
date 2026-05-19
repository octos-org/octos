{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
}:

let
  inherit (lib)
    cleanSource
    concatMapStrings
    concatMapStringsSep
    ;

  src = cleanSource ../../.;
  workspaceToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));

  skillCrates = [
    "news_fetch"
    "deep-search"
    "deep-crawl"
    "send-email"
    "account-manager"
    "clock"
    "weather"
  ];
  skillBins = [
    "news_fetch"
    "deep-search"
    "deep_crawl"
    "send_email"
    "account_manager"
    "clock"
    "weather"
  ];
in

rustPlatform.buildRustPackage {
  inherit src;
  pname = "octos-app-skills";
  version = workspaceToml.workspace.package.version;

  cargoLock.lockFile = src + "/Cargo.lock";

  doCheck = false;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  buildPhase = ''
    runHook preBuild
    cargo build --release ${concatMapStrings (crate: " -p " + crate) skillCrates}
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    ${concatMapStringsSep "\n" (bin: "install -Dm755 target/release/${bin} $out/bin/${bin}") skillBins}
    runHook postInstall
  '';

  passthru = {
    inherit skillBins skillCrates;
  };
}
