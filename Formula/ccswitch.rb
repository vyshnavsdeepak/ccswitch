class Ccswitch < Formula
  desc "Multi-account switcher for Claude Code"
  homepage "https://github.com/vyshnavsdeepak/ccswitch"
  version "0.2.0" # VERSION
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/vyshnavsdeepak/ccswitch/releases/download/v0.2.0/ccswitch-aarch64-apple-darwin.tar.gz" # ARM_URL
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # ARM_SHA
    end

    on_intel do
      url "https://github.com/vyshnavsdeepak/ccswitch/releases/download/v0.2.0/ccswitch-x86_64-apple-darwin.tar.gz" # X86_URL
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # X86_SHA
    end
  end

  def install
    bin.install "ccswitch"
  end

  test do
    system "#{bin}/ccswitch", "--version"
  end
end
