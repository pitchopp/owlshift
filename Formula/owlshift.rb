# Rendered by scripts/release/homebrew-formula.sh for release 0.1.1.
class Owlshift < Formula
  desc "Coding agents that work your backlog and ask on the ticket"
  homepage "https://github.com/pitchopp/owlshift"
  version "0.1.1"
  license "Apache-2.0"

  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.1.1/owlshift-0.1.1-aarch64-apple-darwin.tar.gz"
      sha256 "dd6c962e2a82fa58ce5a29bbb7587774f1b696b0c579734f6438564f8065339a"
    end
    on_intel do
      url "https://github.com/pitchopp/owlshift/releases/download/v0.1.1/owlshift-0.1.1-x86_64-apple-darwin.tar.gz"
      sha256 "21276a25dab28c094fe92a87b29a49a5d4b512214a069452cd7120dce909e1e8"
    end
  end

  def install
    bin.install "owlshift"
  end

  test do
    assert_match "owlshift #{version}", shell_output("#{bin}/owlshift --version")
  end
end
