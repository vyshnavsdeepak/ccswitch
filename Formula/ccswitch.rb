class Ccswitch < Formula
  desc "Multi-account switcher for Claude Code"
  homepage "https://github.com/vyshnavsdeepak/ccswitch"
  version "VERSION_PLACEHOLDER"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/vyshnavsdeepak/ccswitch/releases/download/TAG_PLACEHOLDER/ccswitch-aarch64-apple-darwin.tar.gz"
      sha256 "ARM_SHA_PLACEHOLDER"
    end

    on_intel do
      url "https://github.com/vyshnavsdeepak/ccswitch/releases/download/TAG_PLACEHOLDER/ccswitch-x86_64-apple-darwin.tar.gz"
      sha256 "X86_SHA_PLACEHOLDER"
    end
  end

  def install
    bin.install "ccswitch"
  end

  test do
    system "#{bin}/ccswitch", "--version"
  end
end
