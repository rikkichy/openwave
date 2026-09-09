#!/bin/bash
# Verify three x86-64 native payloads against one source; emit final manifests once.
set -euo pipefail
fail() { printf '%s\n' "$*" >&2; exit 2; }
SOURCE= INPUT= OUT=
while (($#)); do
    (($# >= 2)) || fail "missing value for $1"
    case "$1" in
        --source-dir) SOURCE=$2 ;; --input-dir) INPUT=$2 ;; --output-dir) OUT=$2 ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
[[ -d "$SOURCE" && -d "$INPUT" && -n "$OUT" ]] || fail 'source, input and output directories are required'
SOURCE=$(realpath -- "$SOURCE"); INPUT=$(realpath -- "$INPUT")
mkdir -p -- "$OUT"; OUT=$(realpath -- "$OUT")
shopt -s nullglob dotglob
entries=("$OUT"/*)
((${#entries[@]} == 0)) || fail 'output directory must be empty'
# Reject arbitrary checksum paths before sha256sum opens anything. Every input
# must be a regular non-symlink file, explicitly covered by its own manifest.
verify_and_copy() {
    local directory=$1 manifest=$2 digest filename extra
    [[ -f "$directory/$manifest" && ! -L "$directory/$manifest" ]] || fail "missing manifest: $manifest"
    local -A listed=()
    while read -r digest filename extra; do
        [[ "$digest" =~ ^[0-9a-f]{64}$ && "$filename" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ && -z "$extra" ]] || fail 'invalid checksum manifest entry'
        [[ -z ${listed[$filename]+yes} && "$filename" != "$manifest" ]] || fail 'duplicate checksum entry'
        [[ -f "$directory/$filename" && ! -L "$directory/$filename" ]] || fail 'nonregular artifact'
        listed[$filename]=1
    done < "$directory/$manifest"
    ((${#listed[@]} > 0)) || fail 'empty checksum manifest'
    (cd "$directory" && sha256sum --check "$manifest") || fail 'artifact digest mismatch'
    local path name
    for path in "$directory"/*; do
        name=${path##*/}
        [[ "$name" == "$manifest" || -n ${listed[$name]+yes} ]] || fail "unchecksummed artifact: $name"
        [[ -f "$path" && ! -L "$path" && ! -e "$OUT/$name" ]] || fail "colliding or nonregular artifact: $name"
        cp -- "$path" "$OUT/$name"
    done
}
verify_and_copy "$SOURCE" source.sha256
V=$(cat "$SOURCE/version.txt")
[[ "$V" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || fail 'invalid release version'
[[ -f "$SOURCE/openwave-$V.tar.gz" && -f "$SOURCE/PKGBUILD" && -f "$SOURCE/source-provenance.txt" ]] || fail 'incomplete source artifact'
DIGEST=$(sha256sum "$SOURCE/openwave-$V.tar.gz"); DIGEST=${DIGEST%% *}
inputs=("$INPUT"/*)
((${#inputs[@]} == 3)) || fail 'exactly three native x86_64 distro artifacts are required'
arch=x86_64
for distro in ubuntu24.04 debian13 fedora43; do
    name="native-$distro-$arch"
    directory="$INPUT/$name"
    [[ -d "$directory" && ! -L "$directory" ]] || fail "missing $name"
    verify_and_copy "$directory" "$name.sha256"
    expected=$(printf 'source_sha256=%s\ndistro=%s\narch=%s\nrust=1.98.1' "$DIGEST" "$distro" "$arch")
    [[ $(cat "$directory/build-provenance-$distro-$arch.txt") == "$expected" ]] || fail "source provenance mismatch: $name"
    if [[ "$distro" == fedora43 ]]; then
        payload="openwave-$V-1.fc43.$arch.rpm"
    else
        payload="openwave_${V}-1${distro}_amd64.deb"
    fi
    [[ -f "$directory/$payload" ]] || fail "missing expected payload: $payload"
    files=("$directory"/*)
    ((${#files[@]} == 4)) || fail "unexpected payloads in $name"
done
cd "$OUT"
sha256sum * > sha256sums.txt
# Publish exactly this list (including the checksum manifest), never shell globs
# selecting stale files. The list cannot hash itself and is not self-listed.
printf '%s\n' * > release-objects
printf 'Assembled three x86_64 native packages from openwave-%s.tar.gz; no publication performed.\n' "$V"
