#!/bin/sh
# Prints the Homebrew formula of release VERSION from the release's
# SHA256SUMS (OWL-540). The repository is its own tap: once a release is
# published, the maintainer commits this output as Formula/owlshift.rb on
# main (CONTRIBUTING.md, "Releasing"), and people install with
#
#   brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift
#   brew install owlshift
#
#   sh scripts/release/homebrew-formula.sh VERSION SHA256SUMS

set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: homebrew-formula.sh VERSION SHA256SUMS" >&2
    exit 2
fi
version=$1
sums=$2
base="https://github.com/pitchopp/owlshift/releases/download/v$version"

# The checksum SHA256SUMS lists for the archive of target $1: exactly one
# line naming it, with 64 lowercase hexadecimal digits. install.sh reads
# SHA256SUMS the same way.
checksum() {
    archive="owlshift-$version-$1.tar.gz"
    found=$(awk -v name="$archive" '$2 == name { print $1 }' "$sums")
    case $found in
        *[!0-9a-f]*) found= ;;
    esac
    if [ "${#found}" -ne 64 ]; then
        echo "error: $sums does not list $archive once with a SHA-256 checksum" >&2
        exit 1
    fi
    printf '%s' "$found"
}

arm=$(checksum aarch64-apple-darwin)
intel=$(checksum x86_64-apple-darwin)

cat <<EOF
# Rendered by scripts/release/homebrew-formula.sh for release $version.
class Owlshift < Formula
  desc "Coding agents that work your backlog and ask on the ticket"
  homepage "https://github.com/pitchopp/owlshift"
  version "$version"
  license "Apache-2.0"

  depends_on :macos

  on_macos do
    on_arm do
      url "$base/owlshift-$version-aarch64-apple-darwin.tar.gz"
      sha256 "$arm"
    end
    on_intel do
      url "$base/owlshift-$version-x86_64-apple-darwin.tar.gz"
      sha256 "$intel"
    end
  end

  def install
    bin.install "owlshift"
  end

  test do
    assert_match "owlshift #{version}", shell_output("#{bin}/owlshift --version")
  end
end
EOF
