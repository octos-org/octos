{
  mkShellNoCC,
  nix,
  nixfmt,
  statix,
  openssl,
  pkg-config,
  cargo,
  clippy,
  rust-analyzer,
  rustc,
  rustfmt,
}:
mkShellNoCC {
  packages = [
    nix
    nixfmt
    statix

    openssl
    pkg-config
    cargo
    clippy
    rust-analyzer
    rustc
    rustfmt
  ];
}
