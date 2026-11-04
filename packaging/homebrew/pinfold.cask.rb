# The maintainer creates and publishes the Homebrew tap.
cask "pinfold" do
  version "0.0.0" # RELEASE: replace with the tag version without the leading v.
  # RELEASE: replace from the signed DMG's SHA-256 once it is built.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/0xPiranhaCodes/pinfold.sh/releases/download/v#{version}/PinFold.dmg"
  name "PinFold"
  desc "Terminal built for parallel coding agents"
  homepage "https://pinfold.sh"

  app "PinFold.app"
end
