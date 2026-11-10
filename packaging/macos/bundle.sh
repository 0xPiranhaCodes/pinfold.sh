#!/bin/sh
# Builds and signs PinFold.app from binaries already built. Run from the repository root:
#   sh packaging/macos/bundle.sh <pinfold> <pinfold-core> <pinfold-window> <output directory> [identity]
# The identity defaults to `-`, an ad-hoc signature; the tag build passes its Developer ID.
set -eu

pinfold=$1
core=$2
window=$3
app=$4/PinFold.app
identity=${5:--}
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
# Apple reads at most three integers in each key: CFBundleVersion is 1.2.(300+N) for 1.2.3-beta.N, 1.2.399 for 1.2.3.
short=${version%%-*}
case $version in
  "$short") beta=99 ;;
  "$short"-beta.99) beta= ;;
  "$short"-beta.[1-9] | "$short"-beta.[1-9][0-9]) beta=${version#"$short"-beta.} ;;
  *) beta= ;;
esac
if [ -z "$beta" ] || ! printf '%s\n' "$short" | grep -Eqx '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'; then
  echo "version $version is not MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-beta.N, N from 1 to 98" >&2
  exit 1
fi
build=${short%.*}.$((${short##*.} * 100 + beta))

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed -e "s/@VERSION@/$version/g" -e "s/@SHORT_VERSION@/$short/g" -e "s/@BUILD@/$build/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint -s "$app/Contents/Info.plist"
# install, not cp: binaries downloaded from another job's artifact lose their mode.
install -m 0755 "$pinfold" "$core" "$window" "$app/Contents/MacOS/"
cp packaging/macos/AppIcon.icns packaging/macos/Credits.rtf crates/pinfold-render/fonts/OFL.txt LICENSE-MIT LICENSE-APACHE \
  "$app/Contents/Resources/"
mkdir "$app/Contents/Resources/theme-licenses"
cp crates/pinfold-config/themes/LICENSE-*.txt "$app/Contents/Resources/theme-licenses/"
# The standard About panel shows Credits.rtf and the copyright only if both are there and parse.
textutil -convert txt -stdout "$app/Contents/Resources/Credits.rtf" | grep -q 'pinfold agent'
plutil -extract NSHumanReadableCopyright raw "$app/Contents/Info.plist" > /dev/null
# The hardened runtime is what refuses a task port: PT_DENY_ATTACH does not (ADR-0020).
# Notarization also needs it on every executable. An ad-hoc signature cannot be timestamped.
timestamp=--timestamp
[ "$identity" != - ] || timestamp=--timestamp=none
# Inside out: each binary, then the bundle, whose signature seals them.
for bin in pinfold-core pinfold-window pinfold; do
  codesign --force --sign "$identity" --options runtime "$timestamp" "$app/Contents/MacOS/$bin"
done
codesign --force --sign "$identity" --options runtime "$timestamp" "$app"
codesign --verify --strict --deep "$app"
for bin in pinfold-core pinfold-window pinfold; do
  codesign -dv "$app/Contents/MacOS/$bin" 2>&1 | grep -Eq '^CodeDirectory .* flags=[^ ]*runtime' || {
    echo "$bin is not signed with the hardened runtime" >&2
    exit 1
  }
done
echo "$app"
