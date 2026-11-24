# Rendered by scripts/release/homebrew-formula.sh for release 0.3.0.
class Owlshift < Formula
  desc "Coding agents that work your backlog and ask on the ticket"
  homepage "https://github.com/pitchopp/owlshift"
  version "0.3.0"
  license "Apache-2.0"

  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.3.0/owlshift-0.3.0-aarch64-apple-darwin.tar.gz"
      sha256 "932909d226d953c94627414947426cb1db5da9f7021d48f57905b0c8ed85ee1d"
    end
    on_intel do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.3.0/owlshift-0.3.0-x86_64-apple-darwin.tar.gz"
      sha256 "94a9800d5736c40c5422f193f0f1104e32159341086f98dc131958cf9e6e5919"
    end
  end

  def install
    bin.install "owlshift"
  end

  test do
    assert_match "owlshift #{version}", shell_output("#{bin}/owlshift --version")
  end
end
