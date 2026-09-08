PREFIX ?= /usr/local
DESTDIR ?=
PYTHON ?= python3
# A private, Python-version-independent module tree; distributions may override.
SITEPKG ?= $(PREFIX)/share/openwave/site-packages

BINDIR = $(DESTDIR)$(PREFIX)/bin
DATADIR = $(DESTDIR)$(PREFIX)/share
APPDIR = $(DATADIR)/openwave
DESKTOPDIR = $(DATADIR)/applications
DOCDIR = $(DATADIR)/doc/openwave
LICENSEDIR = $(DATADIR)/licenses/openwave
# Keep launchers relocatable when SITEPKG is inside PREFIX (including DESTDIR).
LAUNCHER_SITE = $(patsubst $(PREFIX)/%,$$prefix/%,$(SITEPKG))

.PHONY: install uninstall check-version

check-version:
	$(PYTHON) packaging/version.py

install: check-version
	install -dm755 "$(DESTDIR)$(SITEPKG)/wavexlr" "$(BINDIR)"
	tar --exclude=__pycache__ --exclude='*.pyc' --exclude='*.pyo' -C wavexlr -cf - . | tar -C "$(DESTDIR)$(SITEPKG)/wavexlr" -xf -
	printf '%s\n' '#!/bin/sh' 'prefix=$$(CDPATH= cd -- "$$(dirname -- "$$0")/.." && pwd)' 'export PYTHONPATH="$(LAUNCHER_SITE)$${PYTHONPATH:+:$$PYTHONPATH}"' 'exec "$(PYTHON)" -m wavexlr "$$@"' > "$(BINDIR)/openwave"
	printf '%s\n' '#!/bin/sh' 'prefix=$$(CDPATH= cd -- "$$(dirname -- "$$0")/.." && pwd)' 'export PYTHONPATH="$(LAUNCHER_SITE)$${PYTHONPATH:+:$$PYTHONPATH}"' 'exec "$(PYTHON)" -m wavexlr.daemon "$$@"' > "$(BINDIR)/openwave-daemon"
	chmod 755 "$(BINDIR)/openwave" "$(BINDIR)/openwave-daemon"
	install -Dm644 wavexlr.desktop "$(DESKTOPDIR)/openwave.desktop"
	install -Dm644 openwave-autostart.desktop "$(APPDIR)/openwave-autostart.desktop"
	install -Dm644 wireplumber/51-openwave-wave-xlr.conf "$(APPDIR)/wireplumber/51-openwave-wave-xlr.conf"
	install -Dm644 pipewire/52-openwave-mixes.conf "$(APPDIR)/pipewire/52-openwave-mixes.conf"
	install -Dm644 VERSION "$(APPDIR)/VERSION"
	install -Dm644 com.github.openwave.metainfo.xml "$(DATADIR)/metainfo/com.github.openwave.metainfo.xml"
	install -Dm644 icons/openwave.svg "$(DATADIR)/icons/hicolor/scalable/apps/openwave.svg"
	install -dm755 "$(DATADIR)/icons/hicolor/symbolic/apps" "$(APPDIR)/icons"
	install -m644 icons/*-symbolic.svg "$(DATADIR)/icons/hicolor/symbolic/apps/"
	install -m644 icons/*.svg "$(APPDIR)/icons/"
	install -Dm644 README.md "$(DOCDIR)/README.md"
	install -Dm644 packaging/asset-attribution.txt "$(DOCDIR)/asset-attribution.txt"
	install -Dm644 LICENSE "$(LICENSEDIR)/LICENSE"

uninstall:
	rm -rf "$(DESTDIR)$(SITEPKG)/wavexlr"
	rm -f "$(BINDIR)/openwave" "$(BINDIR)/openwave-daemon" "$(DESKTOPDIR)/openwave.desktop"
	rm -f "$(DATADIR)/metainfo/com.github.openwave.metainfo.xml" "$(DATADIR)/icons/hicolor/scalable/apps/openwave.svg"
	rm -f "$(DATADIR)/icons/hicolor/symbolic/apps/openwave-symbolic.svg" "$(DATADIR)/icons/hicolor/symbolic/apps/openwave-muted-symbolic.svg" "$(DATADIR)/icons/hicolor/symbolic/apps/openwave-attention-symbolic.svg"
	rm -rf "$(APPDIR)" "$(DOCDIR)" "$(LICENSEDIR)"
