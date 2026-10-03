# Homebrew formula for rterm. The release workflow fills in @VERSION@, @URL@
# and @SHA256@ (packaging/render-formula.sh) and publishes the result to
# https://github.com/guitar24t/homebrew-tap; edit this template, not that copy.
class Rterm < Formula
  desc "Persistent terminal sessions that behave like a plain terminal"
  homepage "https://github.com/guitar24t/rterm"
  url "@URL@"
  version "@VERSION@"
  sha256 "@SHA256@"
  license "MIT"

  def install
    bin.install "rterm", "rterm-connect"
    doc.install "README.md"
  end

  test do
    assert_equal "rterm #{version}", shell_output("#{bin}/rterm --version").strip
    assert_match "Choose an rterm session", shell_output("#{bin}/rterm-connect --help")
    ENV["RTERM_SOCKET_DIR"] = (testpath/"sockets").to_s
    assert_equal "[]", shell_output("#{bin}/rterm ls --json").strip
  end
end
