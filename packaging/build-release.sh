#!/bin/bash
# Prepare one source archive. Payload compilation and publication are separate.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
TAG= SNAPSHOT= OUT=
fail() { printf '%s\n' "$*" >&2; exit 2; }
while (($#)); do
    case "$1" in
        --snapshot-dir) (($# >= 2)) || fail 'missing snapshot directory'; SNAPSHOT=$2; shift 2 ;;
        --output-dir) (($# >= 2)) || fail 'missing output directory'; OUT=$2; shift 2 ;;
        --prepare-only) shift ;;
        v*) [[ -z "$TAG" ]] || fail 'only one tag is accepted'; TAG=$1; shift ;;
        *) fail "unknown argument: $1" ;;
    esac
done
[[ $(id -u) != 0 ]] || fail 'Build release sources as an ordinary build user, not root.'
[[ $(rustc --version) == 'rustc 1.98.1 '* ]] || fail 'Rust 1.98.1 is required'
if [[ -n "$SNAPSHOT" ]]; then
    [[ -z "$TAG" && -n "$OUT" ]] || fail 'snapshot mode requires --output-dir and forbids tags'
    SNAPSHOT=$(realpath -- "$SNAPSHOT")
    [[ -d "$SNAPSHOT" ]] || fail 'snapshot directory is missing'
    MODE=snapshot
else
    [[ -n "$TAG" ]] || fail 'normal release preparation requires exact vMAJOR.MINOR.PATCH tag'
    [[ $(git -C "$ROOT" rev-parse "$TAG^{commit}") == $(git -C "$ROOT" rev-parse HEAD) ]] || fail 'tag must identify HEAD'
    [[ -z $(git -C "$ROOT" status --porcelain --untracked-files=normal) ]] || fail 'tagged release requires a clean source checkout'
    MODE=tag
fi
OUT=${OUT:-"$ROOT/dist/source"}
mkdir -p -- "$OUT"
OUT=$(realpath -- "$OUT")
shopt -s nullglob dotglob
entries=("$OUT"/*)
((${#entries[@]} == 0)) || fail 'output directory must be empty'
if [[ -n "$SNAPSHOT" ]]; then
    [[ "$OUT/" != "$SNAPSHOT/"* ]] || fail 'snapshot output must be outside the input tree'
fi
WORK=$(mktemp -d)
trap 'rm -rf -- "$WORK"' EXIT
mkdir "$WORK/source"
if [[ "$MODE" == tag ]]; then
    git -C "$ROOT" archive "$TAG" | tar -xf - -C "$WORK/source"
    COMMIT=$(git -C "$ROOT" rev-parse "$TAG^{commit}")
else
    # Explicit input may contain current untracked rewrite sources, but never
    # private guidance, credentials, build outputs, Cargo caches or VCS state.
    tar -C "$SNAPSHOT" --exclude=.git --exclude=.omp --exclude=.env --exclude='.env.*' \
        --exclude=AGENTS.md --exclude=CLAUDE.md --exclude=.cursorrules \
        --exclude=target --exclude=dist --exclude=.direnv --exclude=result \
        --exclude=__pycache__ --exclude='.pytest_cache' --exclude='.cargo/registry' \
        --exclude='.cargo/git' --exclude='.cargo/.package-cache*' -cf - . | tar -xf - -C "$WORK/source"
    COMMIT=uncommitted-snapshot
fi
cd "$WORK/source"
# Bootstrap the GTK-free utility from the exact staged inputs and locked graph.
# This is source preparation, the only phase allowed to fetch locked crates.
export CARGO_TARGET_DIR="$WORK/bootstrap-target"
cargo build --locked -p openwave-runtime --bin openwave-maintenance
HELPER="$CARGO_TARGET_DIR/debug/openwave-maintenance"
if [[ "$MODE" == tag ]]; then
    V=$("$HELPER" version --file VERSION --tag "$TAG")
else
    V=$("$HELPER" version --file VERSION)
fi
[[ ! -e vendor ]] || fail 'input already contains vendor; provide an unprepared source tree'
# Cargo discovers the existing project configuration. Its emitted replacement
# is semantically merged only in this staging tree, preserving target settings.
cargo vendor --locked --versioned-dirs vendor > "$WORK/vendor-config.toml"
mkdir -p .cargo
if [[ -e .cargo/config && ! -e .cargo/config.toml ]]; then
    mv .cargo/config .cargo/config.toml
elif [[ -e .cargo/config ]]; then
    fail 'both legacy .cargo/config and config.toml exist; resolve ambiguity first'
fi
"$HELPER" merge-vendor-config --existing .cargo/config.toml --emitted "$WORK/vendor-config.toml" --output .cargo/config.toml
cd "$WORK"
mv source "openwave-$V"
SOURCE="openwave-$V.tar.gz"
# Archive the whole prepared tree, including vendor/.cargo-checksum.json files.
tar -czf "$OUT/$SOURCE" "openwave-$V"
DIGEST=$(sha256sum "$OUT/$SOURCE"); DIGEST=${DIGEST%% *}
"$HELPER" render-aur --version "$V" --sha256 "$DIGEST" --output "$OUT/PKGBUILD"
printf '%s\n' "$V" > "$OUT/version.txt"
printf 'mode=%s\ncommit=%s\ntag=%s\narchive=%s\nsha256=%s\n' "$MODE" "$COMMIT" "$TAG" "$SOURCE" "$DIGEST" > "$OUT/source-provenance.txt"
(cd "$OUT" && sha256sum "$SOURCE" PKGBUILD version.txt source-provenance.txt > source.sha256)
printf 'Prepared %s (%s); use build-native.sh then assemble-release.sh. No publication performed.\n' "$OUT/$SOURCE" "$MODE"
