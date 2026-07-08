class Octos < Formula
  desc "Rust-native, API-first Agentic OS server (octos serve + bundled skills)"
  homepage "https://github.com/octos-org/octos"
  version "1.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/octos-org/octos/releases/download/v1.1.0/octos-bundle-aarch64-apple-darwin.tar.gz"
      sha256 "5fcc62da44542ae0289d38d5d0ba5a8bed45598bf6faf7fec98b382276bf384f"
    end
    on_intel do
      odie "octos requires Apple Silicon; no x86_64 macOS build is published"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/octos-org/octos/releases/download/v1.1.0/octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "428d692364949e14d1ec9823c67c049f1c01a7653f928797c72e70d494c8647c"
    end
    on_arm do
      url "https://github.com/octos-org/octos/releases/download/v1.1.0/octos-bundle-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a93c61951facbd394a16502041fe8890161739b5b61254228504469929c21dcd"
    end
  end

  def install
    # The release bundle ships `octos` alongside its 8 skill binaries
    # (news_fetch, deep-search, deep_crawl, send_email, account_manager,
    # voice, clock, weather). At `octos serve` startup, bootstrap discovers
    # those skills as SIBLINGS of the resolved `octos` executable
    # (current_exe().parent()). Keep all of them together in libexec and
    # expose only `octos` on PATH.
    libexec.install Dir["*"]
    # Use an exec WRAPPER, not bin.install_symlink: on macOS,
    # std::env::current_exe() returns the *symlink* path (Darwin
    # _NSGetExecutablePath), so a bin/octos symlink would put exe_dir at bin/
    # and the sibling skills (in libexec) would not be found -> plugin_count=0.
    # `exec` replaces the process image, so current_exe() == libexec/octos and
    # exe_dir == libexec, where the skill binaries live.
    (bin/"octos").write <<~SH
      #!/bin/bash
      exec "#{libexec}/octos" "$@"
    SH
  end

  test do
    assert_match "octos", shell_output("#{bin}/octos --version")
  end
end
