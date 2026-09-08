# Maintainer: rikkichy
# packaging/build-release.sh pins version/source/checksum for the published
# AUR recipe. In a checkout, this recipe builds the local source tree.
pkgname=openwave
pkgver=$(cat "${startdir:-.}/VERSION")
pkgrel=1
pkgdesc="Linux control application for Elgato Wave hardware and PipeWire mixing"
arch=('any')
url="https://github.com/rikkichy/openwave"
license=('MIT')
depends=('python' 'python-gobject' 'gtk4' 'libadwaita>=1.5' 'adwaita-icon-theme' 'libusb' 'pipewire' 'wireplumber' 'alsa-utils' 'libpulse' 'swh-plugins' 'polkit')
makedepends=('make')
source=()
sha256sums=()

package() {
    cd "$startdir"
    make install DESTDIR="$pkgdir" PREFIX=/usr PYTHON=python3
}
