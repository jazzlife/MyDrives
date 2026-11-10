#!/usr/bin/env bash
# Builds MyDrives.app and the disk image users download.
#
#   packaging/macos/make-app.sh 0.1.5 dist \
#     target/aarch64-apple-darwin/release [target/x86_64-apple-darwin/release]
#
# With both directories the executables are made universal, so one download
# covers Apple Silicon and Intel.
set -euo pipefail

version=${1:?version}
out=${2:?output directory}
arm=${3:?arm64 binary directory}
intel=${4:-}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

app="$out/MyDrives.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# The executables are assembled and signed *outside* the bundle. Handing
# codesign a path inside one makes it sign the bundle instead, seal and all -
# and the agent updates itself by replacing these files, which a sealed bundle
# treats as tampering and refuses to launch.
#
# Apple Silicon will not run a Mach-O with no signature at all, and `lipo`
# drops the ad-hoc one the linker put there, so each file gets a new one.
work="$out/.staging"
rm -rf "$work"
mkdir -p "$work"
for binary in mydrivesd mydrives mydrives-ui; do
  if [ -n "$intel" ]; then
    lipo -create "$arm/$binary" "$intel/$binary" -output "$work/$binary"
  else
    cp "$arm/$binary" "$work/$binary"
  fi
  chmod +x "$work/$binary"
  codesign --force --sign - "$work/$binary"
  mv "$work/$binary" "$app/Contents/MacOS/$binary"
done
rmdir "$work"

sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

python3 "$root/packaging/icon.py" "$out" >/dev/null
iconutil -c icns "$out/icon.iconset" -o "$app/Contents/Resources/AppIcon.icns"

# Not `codesign --verify`: pointed at anything inside a bundle it validates the
# whole bundle, including the seal that is missing on purpose. Running one of
# the executables is the check that matters anyway - a signature the kernel
# will not accept shows up as a process that dies instead of printing.
"$app/Contents/MacOS/mydrives" --version >/dev/null
[ ! -e "$app/Contents/_CodeSignature" ] || {
  echo "the bundle got sealed; self-update would break" >&2
  exit 1
}

staging=$out/dmg
rm -rf "$staging"
mkdir -p "$staging"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"

dmg="$out/MyDrives-$version-macos.dmg"
rm -f "$dmg"
hdiutil create -volname "MyDrives $version" -srcfolder "$staging" \
  -ov -format UDZO -quiet "$dmg"
rm -rf "$staging"

echo "$dmg"
