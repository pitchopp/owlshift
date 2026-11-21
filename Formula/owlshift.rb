# Rendered by scripts/release/homebrew-formula.sh for release 0.2.0.
class Owlshift < Formula
  desc "Coding agents that work your backlog and ask on the ticket"
  homepage "https://github.com/pitchopp/owlshift"
  version "0.2.0"
  license "Apache-2.0"

  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.2.0/owlshift-0.2.0-aarch64-apple-darwin.tar.gz"
      sha256 "0950b8f01008104d32896d7ae1d6c4ea2d8811e74c3ea344907a0bf46792c92c"
    end
    on_intel do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.2.0/owlshift-0.2.0-x86_64-apple-darwin.tar.gz"
      sha256 "dce3c47a8b8eb75316c3a76e83c52fd9d987269025b4f54795f16a78cd88fd06"
    end
  end

  def install
    bin.install "owlshift"
  end

  test do
    assert_match "owlshift #{version}", shell_output("#{bin}/owlshift --version")
  end
end
