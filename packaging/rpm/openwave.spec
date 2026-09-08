# Version is supplied by packaging/build-release.sh from the checked VERSION.
Name:           openwave
Version:        %{openwave_version}
Release:        1%{?dist}
Summary:        Elgato Wave control panel and PipeWire mixing matrix
License:        MIT
URL:            https://github.com/rikkichy/openwave
BuildArch:      noarch
Source0:        openwave-%{version}.tar.gz
Requires:       python3 >= 3.10
Requires:       python3-gobject
Requires:       gtk4
Requires:       libadwaita
Requires:       adwaita-icon-theme
Requires:       polkit
Requires:       libusb1
Requires:       pipewire
Requires:       pipewire-utils
Requires:       wireplumber
Requires:       alsa-utils
Requires:       pulseaudio-utils
Requires:       ladspa-swh-plugins

%description
Control Elgato Wave audio hardware and route application and device sources
through independent PipeWire mixes and output devices.

%prep
%autosetup

%install
make install DESTDIR=%{buildroot} PREFIX=/usr PYTHON=python3 \
    SITEPKG=/usr/share/openwave/site-packages

%files
/usr/bin/openwave
/usr/bin/openwave-daemon
/usr/bin/openwave-diag
/usr/bin/openwave-probe
/usr/share/openwave/
/usr/share/applications/openwave.desktop
/usr/share/metainfo/com.github.openwave.metainfo.xml
/usr/share/icons/hicolor/scalable/apps/openwave.svg
/usr/share/icons/hicolor/symbolic/apps/openwave-symbolic.svg
/usr/share/icons/hicolor/symbolic/apps/openwave-muted-symbolic.svg
/usr/share/icons/hicolor/symbolic/apps/openwave-attention-symbolic.svg
%doc /usr/share/doc/openwave/
%license /usr/share/licenses/openwave/
