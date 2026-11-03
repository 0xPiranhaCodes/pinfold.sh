# The maintainer creates and publishes the Homebrew tap.
class Pinfold < Formula
  desc "Terminal built for parallel coding agents"
  homepage "https://pinfold.sh"
  version "0.0.0" # RELEASE: replace with the tag version without the leading v.
  license "MIT OR Apache-2.0"

  depends_on :linux
  depends_on "libxkbcommon"

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/0xPiranhaCodes/pinfold.sh/releases/download/v#{version}/" \
          "pinfold-linux-aarch64.tar.gz"
      # RELEASE: replace from the tarball's .sha256 file.
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/0xPiranhaCodes/pinfold.sh/releases/download/v#{version}/" \
          "pinfold-linux-x86_64.tar.gz"
      # RELEASE: replace from the tarball's .sha256 file.
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "pinfold", "pinfold-core", "pinfold-window"
    doc.install "README.md", "LICENSE-MIT", "LICENSE-APACHE", "OFL.txt"
  end

  test do
    assert_match "usage: pinfold", shell_output("#{bin}/pinfold --help 2>&1", 1)
  end
end
