class Octos < Formula
  desc "Rust-native, API-first Agentic OS server (octos serve + bundled skills)"
  homepage "https://github.com/octos-org/octos"
  version "2.0.3"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/octos-org/octos/releases/download/v2.0.3/octos-bundle-aarch64-apple-darwin.tar.gz"
      sha256 "4fdefe4345fe6fa3079577206be58c3615ce997427e79d3ca9f6885eebbbac31"
    end
    on_intel do
      odie "octos requires Apple Silicon; no x86_64 macOS build is published"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/octos-org/octos/releases/download/v2.0.3/octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "89a361b16a41cffeb060363b65244e1c12543c0525e7ae7d1f09af9987d212cf"
    end
    on_arm do
      url "https://github.com/octos-org/octos/releases/download/v2.0.3/octos-bundle-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0c1891bdc8c5c09fc5eb3c1e8479f7837be6c50cb32d8cb9bddb3315f786d411"
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
    # `Pathname#write` does not set the executable bit, so the wrapper — the only
    # `octos` on PATH — must be chmod'd or `octos` fails with permission denied.
    (bin/"octos").chmod 0755
  end

  test do
    assert_match "octos", shell_output("#{bin}/octos --version")
  end
end
