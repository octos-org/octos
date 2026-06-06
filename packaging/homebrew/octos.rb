class Octos < Formula
  desc "Rust-native, API-first Agentic OS server (octos serve + bundled skills)"
  homepage "https://github.com/octos-org/octos"
  version "__VERSION__"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/octos-org/octos/releases/download/__TAG__/octos-bundle-aarch64-apple-darwin.tar.gz"
      sha256 "__SHA_DARWIN_ARM__"
    end
    on_intel do
      odie "octos requires Apple Silicon; no x86_64 macOS build is published"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/octos-org/octos/releases/download/__TAG__/octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA_LINUX_X64__"
    end
    on_arm do
      url "https://github.com/octos-org/octos/releases/download/__TAG__/octos-bundle-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA_LINUX_ARM__"
    end
  end

  def install
    # The release bundle ships `octos` alongside its 8 skill binaries
    # (news_fetch, deep-search, deep_crawl, send_email, account_manager,
    # voice, clock, weather). At `octos serve` startup, bootstrap discovers
    # those skills as SIBLINGS of the resolved `octos` executable
    # (current_exe().parent()). Keep all of them together in libexec and
    # expose only `octos` on PATH via a symlink — the symlink resolves so
    # current_exe().parent() == libexec and sibling discovery still works.
    libexec.install Dir["*"]
    bin.install_symlink libexec/"octos"
  end

  test do
    assert_match "octos", shell_output("#{bin}/octos --version")
  end
end
