#!/usr/bin/env bash
# package-macos.sh — build a macOS .app bundle and wrap it in a .dmg.
#
# A plain binary is not enough on macOS.  Two things need a real bundle:
#
#   * The camera permission is granted to the *bundle identifier*, not to
#     whatever launched the binary.  A bare command-line binary has no
#     bundle identifier, so macOS asks for camera access every time and
#     the answer is remembered per terminal application.  A proper .app
#     gets a stable identifier, so the operator is asked once.
#
#   * The SDL2 and OpenSSL libraries are built for the Homebrew prefix,
#     so they have to travel with the application.  The bundle carries
#     them in Contents/Frameworks, and the load paths are rewritten to
#     point there — so the .app runs on a machine that has never seen
#     Homebrew.
#
# Usage:
#   ./scripts/package-macos.sh              # build the .app and the .dmg
#   ./scripts/package-macos.sh --app-only   # build the .app, no .dmg
#   ./scripts/package-macos.sh --no-sign    # skip the ad-hoc signature
#
# Environment:
#   VERSION   the version to put in the bundle (default: the crate version)
#   PREFIX    where the bundle is written (default: target/macos)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

APP_ONLY=0
SIGN=1

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --app-only   Build the .app bundle but not the .dmg
  --no-sign    Skip the ad-hoc signature
  -h, --help   Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --app-only) APP_ONLY=1; shift ;;
        --no-sign)  SIGN=0; shift ;;
        -h|--help)  usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    die "this script packages for macOS — run it on a Mac"
fi

# ── Identity ─────────────────────────────────────────────────────────────────

# The bundle identifier is what the camera permission is keyed on, so it
# has to be stable across releases.
BUNDLE_ID="io.tezz.annotator"
APP_NAME="Court Annotator"
# The bundle directory is called after the application; the binary
# inside keeps its own name.
EXECUTABLE="annotator"

: "${VERSION:=$(grep -m1 '^version = ' "$PROJECT_DIR/Cargo.toml" | cut -d'"' -f2)}"
: "${PREFIX:=$TARGET_DIR/macos}"

BUILD_DIR="$PREFIX/build"
APP="$PREFIX/$APP_NAME.app"
# The disk image is named after the artifact, not after the application:
# the release assets follow one naming scheme, and a name without spaces
# is easier to handle in a shell.
DMG="$PREFIX/annotator-$VERSION-macos-arm64.dmg"

info "Packaging $APP_NAME $VERSION"

# ── The binary ───────────────────────────────────────────────────────────────

BINARY="$TARGET_DIR/$TARGET/release/$EXECUTABLE"
if [[ ! -f "$BINARY" ]]; then
    die "no release binary at $BINARY — run: make build-macos"
fi

rm -rf "$APP" "$BUILD_DIR"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources"

# The binary is copied, not linked, so the source tree stays clean.
cp "$BINARY" "$APP/Contents/MacOS/$EXECUTABLE"
chmod +x "$APP/Contents/MacOS/$EXECUTABLE"

# ── Bundle the libraries ─────────────────────────────────────────────────────
#
# Everything the binary links against that is not part of macOS itself
# has to come along.  The list is computed from the binary, so a new
# dependency cannot be forgotten.
FRAMEWORKS="$APP/Contents/Frameworks"

# The whole bundling is done in Python: the path mapping between
# Homebrew's Cellar and opt/ trees, and between a library's own name
# and the name it is referenced under, is fiddly enough that shell
# makes it error-prone.
bundle_dylibs() {
    local root="$1"
    python3 - "$root" "$FRAMEWORKS" <<'PYTHON'
import os
import shutil
import subprocess
import sys

root, frameworks = sys.argv[1], sys.argv[2]


def otool(path):
    out = subprocess.run(
        ["otool", "-L", path], capture_output=True, text=True
    ).stdout
    deps = []
    for line in out.splitlines()[1:]:
        line = line.strip()
        if not line:
            continue
        deps.append(line.split(" (")[0].strip())
    return deps


def is_system(path):
    return path.startswith("/System/") or path.startswith("/usr/lib/")


def resolve(path):
    """The stable Homebrew spelling of a library.

    Homebrew's formulas live in the Cellar, but the libraries refer to
    each other through the opt/ symlinks, which survive an upgrade.
    The opt/ spelling is preferred so the bundled library matches the
    name its siblings expect.
    """
    real = os.path.realpath(path)
    if "/Cellar/" in real:
        head, rest = real.split("/Cellar/", 1)
        keg = rest.split("/", 1)[0]
        opt = os.path.join(head, "opt", keg, rest.split("/", 1)[1])
        if os.path.exists(opt):
            return opt
    return real


def install_name_tool(*args):
    subprocess.run(
        ["install_name_tool", *args], capture_output=True, text=True
    )


# ── Collect ─────────────────────────────────────────────────────────────────

queue = [d for d in otool(root) if not is_system(d)]
seen = {}

while queue:
    current = queue.pop(0)
    resolved = resolve(current)
    if not os.path.isfile(resolved):
        continue
    name = os.path.basename(resolved)
    if name in seen:
        continue
    seen[name] = resolved

    dest = os.path.join(frameworks, name)
    shutil.copy2(resolved, dest)
    os.chmod(dest, 0o755)  # the copy is read-only

    # The library's own id has to point into the bundle, or a library
    # that refers to itself by its old path loads the Homebrew copy.
    install_name_tool("-id", "@rpath/" + name, dest)

    # Every library can find its siblings: they are all in the same
    # directory.
    install_name_tool("-add_rpath", "@loader_path", dest)
    install_name_tool("-add_rpath", "@loader_path/../Frameworks", dest)

    for dep in otool(resolved):
        if is_system(dep):
            continue
        if dep.startswith("@rpath/"):
            bare = dep[len("@rpath/"):]
            src = os.path.join(os.path.dirname(resolved), bare)
            if os.path.isfile(src):
                dest_r = os.path.join(frameworks, bare)
                shutil.copy2(src, dest_r)
                os.chmod(dest_r, 0o755)
                install_name_tool("-id", "@rpath/" + bare, dest_r)
                install_name_tool(
                    "-add_rpath", "@loader_path", dest_r
                )
                install_name_tool(
                    "-add_rpath", "@loader_path/../Frameworks", dest_r
                )
            install_name_tool("-change", dep, "@rpath/" + bare, dest)
            continue

        queue.append(dep)

# ── Rewrite the references ───────────────────────────────────────────────────
#
# This has to be a separate pass: a library may refer to a sibling
# that is only bundled later in the walk, so the references cannot be
# rewritten while the libraries are being collected.

for name, resolved in seen.items():
    dest = os.path.join(frameworks, name)
    for dep in otool(resolved):
        if is_system(dep):
            continue
        if dep.startswith("@rpath/"):
            continue

        # An absolute reference to a Homebrew (or other) library.  Both
        # the path as written and its resolved form are rewritten,
        # because a library may use either spelling.
        dep_resolved = resolve(dep)
        dep_name = os.path.basename(dep_resolved)
        if dep_name in seen:
            install_name_tool("-change", dep, "@rpath/" + dep_name, dest)
            install_name_tool("-change", dep_resolved, "@rpath/" + dep_name, dest)
            continue

        # A library referenced under a name that is not its basename —
        # the version suffix may differ.  Fall back to the name as
        # written.
        local_name = os.path.basename(dep)
        if local_name in seen:
            install_name_tool("-change", dep, "@rpath/" + local_name, dest)

# ── The executable ───────────────────────────────────────────────────────────

install_name_tool("-add_rpath", "@executable_path/../Frameworks", root)
install_name_tool("-add_rpath", "@loader_path/../Frameworks", root)

for dep in otool(root):
    if is_system(dep):
        continue
    if dep.startswith("@rpath/"):
        continue
    dep_resolved = resolve(dep)
    dep_name = os.path.basename(dep_resolved)
    if os.path.isfile(os.path.join(frameworks, dep_name)):
        install_name_tool("-change", dep, "@rpath/" + dep_name, root)
        install_name_tool("-change", dep_resolved, "@rpath/" + dep_name, root)
    else:
        local_name = os.path.basename(dep)
        if os.path.isfile(os.path.join(frameworks, local_name)):
            install_name_tool("-change", dep, "@rpath/" + local_name, root)

print(f"  bundled {len(seen)} libraries")
PYTHON
}

info "Bundling the libraries..."
bundle_dylibs "$APP/Contents/MacOS/$EXECUTABLE"

# ── SDL3 ─────────────────────────────────────────────────────────────────────
#
# The Homebrew SDL2 is `sdl2-compat`, a shim that loads SDL3 at run time
# with dlopen.  Without SDL3 the shim aborts in its constructor, before
# the application gets a chance to run — the failure looks like a hang,
# because the shim shows a modal error dialog.
#
# The shim looks for SDL3 in @loader_path/../Frameworks, which is
# exactly where the bundle puts its libraries.
SDL3_LIB="$(brew --prefix sdl3 2>/dev/null)/lib/libSDL3.dylib"
if [[ -f "$SDL3_LIB" ]]; then
    cp -L "$SDL3_LIB" "$FRAMEWORKS/libSDL3.dylib"
    chmod u+w "$FRAMEWORKS/libSDL3.dylib"
    install_name_tool -id "@rpath/libSDL3.dylib" "$FRAMEWORKS/libSDL3.dylib" 2>/dev/null || true
    install_name_tool -add_rpath "@loader_path" "$FRAMEWORKS/libSDL3.dylib" 2>/dev/null || true
    install_name_tool -add_rpath "@loader_path/../Frameworks" "$FRAMEWORKS/libSDL3.dylib" 2>/dev/null || true
    info "Bundled SDL3"
else
    die "SDL3 not found — sdl2-compat needs it at run time (brew install sdl3)"
fi

# ── Info.plist ───────────────────────────────────────────────────────────────

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>
    <string>$EXECUTABLE</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <!-- The application captures video from a camera. -->
    <key>NSCameraUsageDescription</key>
    <string>The Court Annotator captures video from a camera so the annotations can be drawn over the court feed.</string>
    <!-- A kiosk: the application is meant to run in the foreground. -->
    <key>LSUIElement</key>
    <false/>
</dict>
</plist>
PLIST

# ── Icon ─────────────────────────────────────────────────────────────────────
#
# The icon is drawn with Core Graphics by assets/make-icon.swift, so it
# stays crisp at every size and the source stays readable.  It is
# rendered on every build into the build directory — the .icns is a
# build artefact, and the source tree is left untouched.
ICON_SRC="$BUILD_DIR/AppIcon.icns"
if command -v swift >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
    info "Drawing the icon..."
    ICONSET="$BUILD_DIR/AppIcon.iconset"
    rm -rf "$ICONSET"
    mkdir -p "$BUILD_DIR"
    swift "$PROJECT_DIR/assets/make-icon.swift" "$ICONSET" >/dev/null
    iconutil -c icns "$ICONSET" -o "$ICON_SRC"
    rm -rf "$ICONSET"
    cp "$ICON_SRC" "$APP/Contents/Resources/AppIcon.icns"
    info "Icon: drawn from assets/make-icon.swift"
else
    warn "swift or iconutil not found — the application will have the default icon"
fi

# ── Signature ────────────────────────────────────────────────────────────────
#
# An ad-hoc signature is enough for the application to run on the
# machine that built it and on any other machine, as long as the user
# accepts the "unidentified developer" warning.  A Developer ID
# signature would be needed for the warning to disappear, but that
# needs an Apple developer account, which is out of scope here.
if [[ "$SIGN" == "1" ]]; then
    info "Signing the bundle (ad-hoc)..."
    # The hardened runtime is deliberately NOT enabled.
    #
    # It turns on library validation, which requires every library the
    # process loads to carry the same Team ID as the executable.  An
    # ad-hoc signature has no Team ID — the loader derives one from the
    # signature's hash, so every differently-signed library looks like
    # it comes from a different team and the load is refused with
    # "mapping process and mapped file have different Team IDs".
    #
    # A Developer ID signature would have a real, shared Team ID and
    # would work with the hardened runtime.  Until there is one, the
    # application is signed without it: the user still gets the
    # "unidentified developer" warning, but the application runs.
    #
    # The frameworks are signed first, innermost first: a signature
    # covers the code it is attached to, so the outer bundle's
    # signature would be invalid if a nested one changed.
    #
    # `--deep` is not used: it is deprecated, and it re-signs code that
    # is already signed, which invalidates the outer signature.
    for lib in "$FRAMEWORKS"/*.dylib; do
        [[ -f "$lib" ]] || continue
        codesign --force --sign - \
            --identifier "$BUNDLE_ID.$(basename "$lib")" \
            --timestamp=none \
            "$lib" 2>/dev/null || true
    done

    codesign --force --sign - \
        --identifier "$BUNDLE_ID" \
        --timestamp=none \
        "$APP/Contents/MacOS/$EXECUTABLE"

    codesign --force --sign - \
        --identifier "$BUNDLE_ID" \
        --timestamp=none \
        "$APP"
fi

# ── Verify ───────────────────────────────────────────────────────────────────

if ! otool -L "$APP/Contents/MacOS/$EXECUTABLE" | grep -q "@rpath"; then
    warn "The executable still refers to absolute library paths"
fi

info "Bundle: $APP"
if command -v file >/dev/null 2>&1; then
    file "$APP/Contents/MacOS/$EXECUTABLE" | sed 's/^/  /'
fi

# ── Disk image ───────────────────────────────────────────────────────────────

if [[ "$APP_ONLY" == "1" ]]; then
    info "Skipping the .dmg (--app-only)"
    exit 0
fi

info "Building the disk image..."
DMG_STAGE="$BUILD_DIR/dmg"
rm -rf "$DMG_STAGE"
mkdir -p "$DMG_STAGE"
cp -R "$APP" "$DMG_STAGE/"

# A symlink to /Applications is what makes the window's "drag to
# install" affordance work.
ln -s /Applications "$DMG_STAGE/Applications"

rm -f "$DMG"
# The image is compressed and read-only, which is what a distribution
# disk image should be.
#
# `hdiutil create -srcfolder` is deprecated in favour of
# `diskutil image create from`, but the replacement needs an empty
# volume first, and hdiutil still does the whole job in one step.
# The deprecation warning is harmless.
hdiutil create \
    -volname "$APP_NAME $VERSION" \
    -srcfolder "$DMG_STAGE" \
    -ov -format UDZO "$DMG" >/dev/null 2>&1

rm -rf "$DMG_STAGE"

info "Disk image: $DMG"
if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$DMG" > "$DMG.sha256"
    info "Checksum: $(cat "$DMG.sha256")"
fi
