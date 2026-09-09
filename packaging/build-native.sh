#!/bin/bash
# Build a verified prepared source on a matching native target userspace.
set -euo pipefail
fail() { printf '%s\n' "$*" >&2; exit 2; }
SOURCE= DIGEST= DISTRO= ARCH= OUT=
while (($#)); do
    (($# >= 2)) || fail "missing value for $1"
    case "$1" in
        --source-archive) SOURCE=$2 ;; --sha256) DIGEST=$2 ;;
        --distro) DISTRO=$2 ;; --arch) ARCH=$2 ;; --output-dir) OUT=$2 ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
[[ -f "$SOURCE" && -n "$OUT" && "$DIGEST" =~ ^[a-fA-F0-9]{64}$ ]] || fail 'source, sha256 and output directory are required'
[[ "$ARCH" == x86_64 ]] || fail 'the supported release architecture is x86_64'
[[ $(uname -m) == "$ARCH" ]] || fail 'native matching runner required; emulation is not a native proof'
case "$DISTRO" in
    ubuntu24.04) IMAGE=docker.io/library/ubuntu@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517 ;;
    debian13) IMAGE=docker.io/library/debian@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1 ;;
    fedora43) IMAGE=docker.io/library/fedora@sha256:a651ddf48ea28a06ed4e1e6519f51c9f47e7a5a138722ade87369b8fbb7e5b42 ;;
    *) fail 'distro must be ubuntu24.04, debian13 or fedora43' ;;
esac
SOURCE=$(realpath -- "$SOURCE")
printf '%s  %s\n' "$DIGEST" "$SOURCE" | sha256sum --check --status || fail 'source digest mismatch'
mkdir -p -- "$OUT"
OUT=$(realpath -- "$OUT")
shopt -s nullglob dotglob
entries=("$OUT"/*)
((${#entries[@]} == 0)) || fail 'output directory must be empty'
ENGINE=${CONTAINER_ENGINE:-docker}
# Only the archive and a private output directory enter the container. No host
# devices, session/runtime sockets, credentials, or checkout are mounted.
"$ENGINE" run --rm -i --security-opt=no-new-privileges \
    --mount "type=bind,src=$SOURCE,dst=/input/source.tar.gz,readonly" \
    --mount "type=bind,src=$OUT,dst=/output" \
    -e DISTRO="$DISTRO" -e ARCH="$ARCH" -e SOURCE_SHA256="$DIGEST" \
    -e OUTPUT_UID="$(id -u)" -e OUTPUT_GID="$(id -g)" "$IMAGE" bash -s <<'CONTAINER'
set -euo pipefail
export LC_ALL=C
[[ $(uname -m) == "$ARCH" ]]
printf '%s  /input/source.tar.gz\n' "$SOURCE_SHA256" | sha256sum --check --status
if [[ "$DISTRO" == fedora43 ]]; then
    dnf install -y ca-certificates curl tar xz gzip shadow-utils util-linux \
        gcc gcc-c++ make pkgconf-pkg-config gtk4-devel libadwaita-devel libusb1-devel \
        rpm-build rpmdevtools dnf5-plugins desktop-file-utils libappstream-glib
else
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends ca-certificates curl tar xz-utils \
        build-essential pkg-config libgtk-4-dev libadwaita-1-dev libusb-1.0-0-dev \
        dpkg-dev fakeroot desktop-file-utils appstream util-linux
fi
RUST_SHA=5326b36c53de11d148c8f8dab6553a3d1006c2cfd32123683073fad3c302605b
RUST="rust-1.98.1-$ARCH-unknown-linux-gnu"
curl --fail --location --proto '=https' --tlsv1.2 \
    "https://static.rust-lang.org/dist/2026-09-03/$RUST.tar.xz" -o /tmp/rust.tar.xz
printf '%s  /tmp/rust.tar.xz\n' "$RUST_SHA" | sha256sum --check --status
tar -xJf /tmp/rust.tar.xz -C /tmp
"/tmp/$RUST/install.sh" --prefix=/opt/rust --disable-ldconfig
rm -rf /tmp/rust.tar.xz "/tmp/$RUST"
export PATH=/opt/rust/bin:$PATH
[[ $(rustc --version) == 'rustc 1.98.1 '* ]]
useradd --create-home builder
mkdir /work
# Prepared archives contain one canonical versioned root; reject traversal and
# absolute names before allowing extraction, even after checksum verification.
tar -tzf /input/source.tar.gz > /tmp/members
while IFS= read -r member; do
    [[ "$member" == openwave-*/* && "$member" != /* && "/$member/" != *'/../'* ]] || exit 2
done < /tmp/members
tar --no-same-owner --no-same-permissions -xzf /input/source.tar.gz -C /work
roots=(/work/openwave-*)
[[ ${#roots[@]} == 1 && -d "${roots[0]}" ]]
SRC=${roots[0]}
V=$(cat "$SRC/VERSION")
[[ "$V" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
if [[ "$DISTRO" == fedora43 ]]; then
    # Ask the actual spec for BuildRequires, rather than assuming compiler-only
    # dependencies suffice; repository package versions are recorded below.
    dnf builddep -y --define "openwave_version $V" --define 'dist .fc43' "$SRC/packaging/rpm/openwave.spec"
    rpm -qa --qf '%{NAME} %{VERSION}-%{RELEASE}.%{ARCH}\n' | sort > /tmp/build-packages.txt
else
    dpkg-query -W -f='${binary:Package} ${Version}\n' | sort > /tmp/build-packages.txt
fi
chown -R builder:builder /work
# No dependency resolution is allowed after preparation, including RPM macros.
runuser -u builder -- env PATH="$PATH" DISTRO="$DISTRO" ARCH="$ARCH" SRC="$SRC" \
    CARGO_HOME=/home/builder/.cargo CARGO_NET_OFFLINE=true RUSTFLAGS='' bash -s <<'BUILD'
set -euo pipefail
cd "$SRC"
cargo build --release --frozen --offline -p openwave-runtime --bin openwave-maintenance
HELPER="$SRC/target/release/openwave-maintenance"
V=$("$HELPER" version --file VERSION)
[[ "${SRC##*/}" == "openwave-$V" ]]
mkdir -p /work/artifacts
if [[ "$DISTRO" == fedora43 ]]; then
    RPMROOT=/work/rpmbuild
    mkdir -p "$RPMROOT/SOURCES"
    cp /input/source.tar.gz "$RPMROOT/SOURCES/openwave-$V.tar.gz"
    rpmbuild --define "_topdir $RPMROOT" --define "openwave_version $V" \
        --define 'dist .fc43' -bb packaging/rpm/openwave.spec
    package="$RPMROOT/RPMS/$ARCH/openwave-$V-1.fc43.$ARCH.rpm"
    [[ -f "$package" && ! -L "$package" ]]
    cp "$package" /work/artifacts/
else
    cargo build --release --frozen --offline --workspace --bins
    STAGE=/work/deb
    make install PREFIX=/usr DESTDIR="$STAGE" INSTALL_METHOD=deb \
        BINARY_DIR="$SRC/target/release" CARGO_BUILD_FLAGS='--release --frozen --offline --workspace --bins'
    mkdir -p "$STAGE/DEBIAN" debian
    # dpkg-shlibdeps needs source package context, and examines every ELF rather
    # than relying on a handwritten list of ABI library names.
    printf 'Source: openwave\nSection: sound\nPriority: optional\nMaintainer: rikkichy <rikkichy@users.noreply.github.com>\n\nPackage: openwave\nArchitecture: any\nDescription: OpenWave\n' > debian/control
    SHLIBS=$(dpkg-shlibdeps -O -e"$STAGE/usr/bin/openwave" \
        -e"$STAGE/usr/bin/openwave-daemon" -e"$STAGE/usr/bin/openwave-diag" \
        -e"$STAGE/usr/bin/openwave-probe" -e"$STAGE/usr/libexec/openwave-maintenance")
    [[ "$SHLIBS" == shlibs:Depends=* ]]
    DEBARCH=$(dpkg --print-architecture)
    [[ "$DEBARCH" == amd64 ]] || { printf '%s\n' 'the supported Debian release architecture is amd64' >&2; exit 2; }
    cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: openwave
Version: $V-1$DISTRO
Section: sound
Priority: optional
Architecture: $DEBARCH
Depends: ${SHLIBS#shlibs:Depends=}, adwaita-icon-theme, pipewire, pipewire-bin, wireplumber, alsa-utils, pulseaudio-utils, swh-plugins, pkexec
Maintainer: rikkichy <rikkichy@users.noreply.github.com>
Homepage: https://github.com/rikkichy/openwave
Description: Elgato Wave control panel and PipeWire mixing matrix
 Route application and device sources to independent mixes and outputs.
CONTROL
    desktop-file-validate "$STAGE/usr/share/applications/openwave.desktop"
    appstreamcli validate --no-net "$STAGE/usr/share/metainfo/com.github.openwave.metainfo.xml"
    dpkg-deb --build --root-owner-group "$STAGE" "/work/artifacts/openwave_${V}-1${DISTRO}_${DEBARCH}.deb"
fi
BUILD
cp /tmp/build-packages.txt "/work/artifacts/build-packages-$DISTRO-$ARCH.txt"
printf 'source_sha256=%s\ndistro=%s\narch=%s\nrust=1.98.1\n' "$SOURCE_SHA256" "$DISTRO" "$ARCH" > "/work/artifacts/build-provenance-$DISTRO-$ARCH.txt"
(cd /work/artifacts && sha256sum * > "native-$DISTRO-$ARCH.sha256")
cp /work/artifacts/* /output/
chown -R "$OUTPUT_UID:$OUTPUT_GID" /output
CONTAINER
