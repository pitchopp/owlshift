# Rendered by scripts/release/homebrew-formula.sh for release 0.1.0.
class Owlshift < Formula
  desc "Coding agents that work your backlog and ask on the ticket"
  homepage "https://github.com/pitchopp/owlshift"
  version "0.1.0"
  license "Apache-2.0"

  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.1.0/owlshift-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "aeeae17fbf491fbb2d15d9a920442f85ed7f3c771afe76ce1b315250b9455399"
    end
    on_intel do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.1.0/owlshift-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "dceceddadf33fe05de509f752c0dca857cf4f0dbf53ba6db90d5b1cdf5ce301b"
    end
  end

  def install
    bin.install "owlshift"
  end

  test do
    assert_match "owlshift #{version}", shell_output("#{bin}/owlshift --version")
  end
end
