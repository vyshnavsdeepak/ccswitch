class Ccswitch < Formula
  desc "Multi-account switcher for Claude Code"
  homepage "https://github.com/vyshnavsdeepak/ccswitch"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/vyshnavsdeepak/ccswitch/releases/download/v0.1.0/ccswitch-aarch64-apple-darwin.tar.gz"
      sha256 "13aa687fcc2f79b897b999c808b0cda4821b18995861335bcd4114b67ed9b60e"
    end

    on_intel do
      url "https://github.com/vyshnavsdeepak/ccswitch/releases/download/v0.1.0/ccswitch-x86_64-apple-darwin.tar.gz"
      sha256 "590f08b238d9d7a7dc3cf45eef64170c0816a37975d11b68de26ca2293e8b7f5"
    end
  end

  def install
    bin.install "ccswitch"
  end

  test do
    system "#{bin}/ccswitch", "--version"
  end
end
