{
  mkShell,
  nix,
  nixfmt,
  statix,
  cargo,
  clippy,
  rust-analyzer,
  rustc,
  rustfmt,
  pkg-config,
  openssl,
}:
mkShell {
  packages = [
    nix
    nixfmt
    statix

    cargo
    clippy
    rust-analyzer
    rustc
    rustfmt

    pkg-config
    openssl
  ];
}
