#!/usr/bin/env bash
# Native, per-architecture proof in a disposable Flatpak installation. Nothing
# is installed into the caller's existing Flatpak installation or host prefix.
set -euo pipefail
umask 077

fail() { printf 'flatpak-build: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: build.sh --source-archive FILE --sha256 HEX --arch x86_64 --output-dir DIR'
}
archive= digest= arch= output=
while (($#)); do
    case "$1" in
        --source-archive|--sha256|--arch|--output-dir)
            (($# >= 2)) || { usage >&2; exit 2; }
            case "$1" in
                --source-archive) archive=$2 ;;
                --sha256) digest=$2 ;;
                --arch) arch=$2 ;;
                --output-dir) output=$2 ;;
            esac
            shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done
[[ -n "$archive" && -n "$output" && "$digest" =~ ^[[:xdigit:]]{64}$ ]] || { usage >&2; exit 2; }
[[ "$arch" == x86_64 ]] || { usage >&2; exit 2; }
((EUID != 0)) || fail 'Build as an ordinary user, never root.'
[[ "$(uname -m)" == "$arch" ]] || fail "A native $arch runner is required; emulation/evaluation is not a runtime proof."
for tool in flatpak flatpak-builder ostree jq curl git sha256sum realpath; do
    command -v "$tool" >/dev/null || fail "Missing build tool: $tool"
done
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
archive=$(realpath -e -- "$archive")
[[ -f "$archive" ]] || fail 'Source archive is not a regular file.'
digest=${digest,,}
actual=$(sha256sum -- "$archive")
[[ "${actual%% *}" == "$digest" ]] || fail 'Source archive SHA256 mismatch.'
[[ ! -L "$output" ]] || fail 'Output directory must not be a symlink.'
mkdir -p -- "$output"
output=$(realpath -e -- "$output")
shopt -s nullglob dotglob
entries=("$output"/*)
((${#entries[@]} == 0)) || fail 'Output directory must be empty.'
shopt -u nullglob dotglob
work=$(mktemp -d "$output/builder.XXXXXXXX")
# Keep the private installation and all logs on failure for diagnosis. Removing
# this output directory later is sufficient cleanup; no daemon/service is used.
export HOME="$work/home"
export XDG_CONFIG_HOME="$HOME/config" XDG_DATA_HOME="$HOME/data"
export XDG_CACHE_HOME="$HOME/cache" XDG_STATE_HOME="$HOME/state"
export XDG_RUNTIME_DIR="$work/runtime"
export FLATPAK_USER_DIR="$work/flatpak" FLATPAK_SYSTEM_DIR="$work/empty-system"
export FLATPAK_CONFIG_DIR="$work/flatpak-config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null LC_ALL=C
unset DBUS_SESSION_BUS_ADDRESS DISPLAY WAYLAND_DISPLAY PULSE_SERVER PIPEWIRE_REMOTE
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" \
    "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR" "$FLATPAK_SYSTEM_DIR" "$FLATPAK_CONFIG_DIR"
cd "$work"
exec > >(tee "$output/build.log") 2>&1
trap 'status=$?; if ((status != 0)); then printf "FAILED (exit %s); private evidence retained in %s\n" "$status" "$work" >&2; fi' EXIT
printf 'Native runner: %s\nSource SHA256: %s\nPrivate builder: %s\n' "$arch" "$digest" "$work"
flatpak --version
flatpak-builder --version

platform_commit=545da92354a265d2c3572c91c39ac14dd7e74f9d8f9b66744ad50f478d2497c5
sdk_commit=c87589be513db588f67de1a27879315dc9697ed2bd8467bd3d55860bf4da2f42
rust_commit=09784a9989ae5a7fda6a2280033ca9aa32aac6907961da8b18be4590dd261507
platform_ref="runtime/org.gnome.Platform/$arch/50"
sdk_ref="runtime/org.gnome.Sdk/$arch/50"
rust_ref="runtime/org.freedesktop.Sdk.Extension.rust-stable/$arch/25.08"
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
pin_ref() {
    local ref=$1 commit=$2
    # Prefetch first, then deploy the exact builder commit. Never confuse
    # 'flatpak pin' (garbage-collection protection) with a version lock.
    flatpak install --user --noninteractive --no-related --no-deps flathub "$ref"
    flatpak update --user --noninteractive --no-related --no-deps --commit="$commit" "$ref"
    [[ "$(flatpak info --user --show-commit "$ref")" == "$commit" ]] || fail "Wrong deployed commit for $ref"
    printf '%s %s\n' "$commit" "$ref" >> "$output/builder-commits.txt"
}
pin_ref "$platform_ref" "$platform_commit"
pin_ref "$sdk_ref" "$sdk_commit"
pin_ref "$rust_ref" "$rust_commit"
flatpak info --user --show-metadata "$sdk_ref" > "$output/sdk-metadata.ini"
# GNOME 50 must select the ABI-compatible 25.08 extension branch itself.
extension_section=false
extension_branch= extension_directory= extension_subdirectories=
while IFS='=' read -r key value; do
    case "$key" in
        '[Extension org.freedesktop.Sdk.Extension]') extension_section=true ;;
        '['*) extension_section=false ;;
        *)
            if $extension_section; then
                key=${key//[[:space:]]/}
                value=${value//[[:space:]]/}
                case "$key" in
                    version) extension_branch=$value ;;
                    directory) extension_directory=$value ;;
                    subdirectories) extension_subdirectories=$value ;;
                esac
            fi ;;
    esac
done < "$output/sdk-metadata.ini"
[[ $extension_branch == 25.08 && $extension_directory == lib/sdk && $extension_subdirectories == true ]] \
    || fail 'Pinned GNOME 50 SDK does not select the lib/sdk Rust extension on branch 25.08.'
flatpak info --user --show-metadata "$rust_ref" > "$output/rust-metadata.ini"
ostree --repo="$FLATPAK_USER_DIR/repo" show --list-metadata-keys "$rust_commit" > "$output/rust-commit-keys.txt"
fallback=false
while IFS= read -r key; do
    case "$key" in
        ostree.endoflife|ostree.endoflife-rebase)
            ostree --repo="$FLATPAK_USER_DIR/repo" show --print-metadata-key="$key" "$rust_commit"
            fallback=true ;;
    esac
done < "$output/rust-commit-keys.txt"
probe_compiler() {
    local directory=$1
    flatpak build-init --arch="$arch" --writable-sdk \
        --sdk-extension=org.freedesktop.Sdk.Extension.rust-stable \
        "$directory" com.github.openwave.CompilerCheck org.gnome.Sdk org.gnome.Platform 50
    flatpak build --unshare=network --unshare=ipc --nodevice=all \
        --nosocket=session-bus --nosocket=system-bus --nosocket=wayland \
        --nosocket=x11 --nosocket=pulseaudio --nofilesystem=host \
        "$directory" /usr/lib/sdk/rust-stable/bin/rustc --version
}
if probe_compiler "$work/compiler-check" > "$output/compiler-before.txt" 2>&1; then
    compiler_before=$(cat "$output/compiler-before.txt")
    if [[ ! "$compiler_before" =~ (^|$'\n')rustc\ 1\.98\.1\  ]]; then fallback=true; fi
else
    fallback=true
fi
cat "$output/compiler-before.txt"
if $fallback; then
    printf '%s\n' 'Building the pinned 25.08 Rust extension locally (EOL or wrong/unavailable compiler).'
    extension_source_commit=3eb7abe30d33d528299ec95e32a64eb70aafda32
    extension_source="$work/rust-extension-source"
    git init "$extension_source"
    git -C "$extension_source" remote add origin https://github.com/flathub/org.freedesktop.Sdk.Extension.rust-stable.git
    git -C "$extension_source" fetch --depth=1 origin "$extension_source_commit"
    git -C "$extension_source" checkout --detach FETCH_HEAD
    [[ "$(git -C "$extension_source" rev-parse HEAD)" == "$extension_source_commit" ]] || fail 'Rust extension source commit mismatch.'
    # Build the complete upstream extension, including its pinned auxiliary
    # tools, using the already pinned GNOME 50 SDK (the 25.08 ABI junction).
    # Avoid downloading an additional unpinned freedesktop SDK. Branch and all
    # upstream source hashes stay unchanged.
    jq '.runtime="org.gnome.Sdk" | .sdk="org.gnome.Sdk" | .["runtime-version"]="50"' \
        "$extension_source/org.freedesktop.Sdk.Extension.rust-stable.json" > "$extension_source/builder.json"
    jq -e --arg arch "$arch" --arg digest 5326b36c53de11d148c8f8dab6553a3d1006c2cfd32123683073fad3c302605b \
        '.branch == "25.08" and any(.modules[] | select(.name == "rust") | .sources[];
        .url == ("https://static.rust-lang.org/dist/2026-09-03/rust-1.98.1-" + $arch + "-unknown-linux-gnu.tar.xz") and .sha256 == $digest)' \
        "$extension_source/builder.json" >/dev/null
    flatpak-builder --user --arch="$arch" --state-dir="$work/extension-state" \
        --download-only "$work/extension-build" "$extension_source/builder.json"
    flatpak-builder --user --arch="$arch" --state-dir="$work/extension-state" \
        --disable-download --sandbox --disable-rofiles-fuse --repo="$work/extension-repo" \
        "$work/extension-build" "$extension_source/builder.json"
    # Replace only the private builder's extension, not a host installation.
    flatpak uninstall --user --noninteractive --no-related "$rust_ref"
    flatpak remote-add --user --no-gpg-verify openwave-local-rust "$work/extension-repo"
    flatpak install --user --noninteractive --no-related --no-deps openwave-local-rust "$rust_ref"
    printf '%s %s (local build from %s)\n' "$(flatpak info --user --show-commit "$rust_ref")" \
        "$rust_ref" "$extension_source_commit" >> "$output/builder-commits.txt"
fi
probe_compiler "$work/compiler-final" > "$output/compiler-final.txt" 2>&1
cat "$output/compiler-final.txt"
compiler_final=$(cat "$output/compiler-final.txt")
[[ "$compiler_final" =~ (^|$'\n')rustc\ 1\.98\.1\  ]] || fail 'The builder must execute rustc 1.98.1; no compiler substitution is allowed.'

# Consume the same prepared archive used by the native packages. No checkout,
# cargo cache or network dependency resolution is substituted for its vendor tree.
flatpak-builder --show-manifest "$script_dir/com.github.openwave.yml" > "$work/manifest-template.json"
jq --arg archive "$archive" --arg digest "$digest" '
    .modules |= map(if .name == "openwave" then
        .sources = [{type:"archive", path:$archive, sha256:$digest}]
        else . end)
' "$work/manifest-template.json" > "$output/manifest.json"
flatpak-builder --user --arch="$arch" --state-dir="$work/app-state" \
    --download-only "$work/app-build" "$output/manifest.json"
flatpak-builder --user --arch="$arch" --state-dir="$work/app-state" \
    --disable-download --sandbox --disable-rofiles-fuse --repo="$output/repo" \
    "$work/app-build" "$output/manifest.json"
flatpak build-bundle --arch="$arch" "$output/repo" "$output/openwave-$arch.flatpak" com.github.openwave
flatpak remote-add --user --no-gpg-verify openwave-built "$output/repo"
flatpak install --user --noninteractive --no-related --no-deps openwave-built "app/com.github.openwave/$arch/master"
# Run the actual installed package, but strip ALL its normal permissions for
# informational/plugin proof: no host USB, audio sockets, buses or services.
flatpak run --user --arch="$arch" --sandbox --command=sh com.github.openwave -ec '
    cd /tmp
    version=$(cat /app/share/openwave/VERSION)
    test "$(/app/bin/openwave --version)" = "openwave $version"
    test "$(/app/bin/openwave-daemon --version)" = "openwave-daemon $version"
    test "$(/app/bin/openwave-diag --version)" = "openwave-diag $version"
    /app/bin/openwave --help
    /app/bin/openwave-daemon --help
    /app/bin/openwave-diag --help
    /app/bin/openwave-probe --help
    test -s /app/share/openwave/style.css
    for tool in pactl wpctl pw-top pw-cat pw-dump pw-link pw-cli pw-loopback pipewire amixer aplay; do
        test -x "/app/bin/$tool"
    done
    test -f /app/lib/ladspa/gate_1410.so
    test -f /app/lib/ladspa/sc4m_1916.so
    test -f /app/lib/pipewire-0.3/libpipewire-module-filter-chain.so
    # ldd -r resolves plugin symbols without initializing audio or USB.
    for plugin in /app/lib/ladspa/gate_1410.so /app/lib/ladspa/sc4m_1916.so; do
        resolved=$(ldd -r "$plugin" 2>&1)
        printf "%s\n" "$resolved"
        case "$resolved" in *"not found"*|*"undefined symbol"*) exit 1 ;; esac
    done
' | tee "$output/installed-proof.txt"
sha256sum "$output/openwave-$arch.flatpak" > "$output/sha256sums.txt"
printf 'PASSED: native %s Flatpak build and installed informational/plugin proof.\n' "$arch" | tee "$output/result.txt"
printf 'No physical USB, graphical UI or live audio verification is claimed.\nPrivate builder/evidence retained: %s\n' "$work"
