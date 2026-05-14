PREFIX ?= /usr/local
DESTDIR ?=
PYTHON ?= python3

BINDIR = $(DESTDIR)$(PREFIX)/bin
DATADIR = $(DESTDIR)$(PREFIX)/share
APPDIR = $(DATADIR)/openwave
DESKTOPDIR = $(DATADIR)/applications
DOCDIR = $(DATADIR)/doc/openwave
LICENSEDIR = $(DATADIR)/licenses/openwave

SITEPKG := $(shell $(PYTHON) -c "import site; print(site.getsitepackages()[0])")

.PHONY: install uninstall

install:
	install -dm755 $(DESTDIR)$(SITEPKG)/wavexlr
	install -m644 wavexlr/__init__.py wavexlr/__main__.py wavexlr/app.py wavexlr/audio.py wavexlr/daemon.py wavexlr/device.py wavexlr/mixmatrix.py wavexlr/service.py wavexlr/setup.py wavexlr/style.css wavexlr/tray.py $(DESTDIR)$(SITEPKG)/wavexlr/
	install -dm755 $(BINDIR)
	printf '#!/bin/sh\nexec %s -m wavexlr "$$@"\n' "$(PYTHON)" > $(BINDIR)/openwave
	chmod 755 $(BINDIR)/openwave
	install -Dm644 wavexlr.desktop $(DESKTOPDIR)/openwave.desktop
	install -Dm644 wireplumber/51-openwave-wave-xlr.conf $(APPDIR)/wireplumber/51-openwave-wave-xlr.conf
	install -Dm644 README.md $(DOCDIR)/README.md
	install -Dm644 LICENSE $(LICENSEDIR)/LICENSE

uninstall:
	rm -rf $(DESTDIR)$(SITEPKG)/wavexlr
	rm -f $(BINDIR)/openwave
	rm -f $(DESKTOPDIR)/openwave.desktop
	rm -rf $(APPDIR)
	rm -rf $(DOCDIR)
	rm -rf $(LICENSEDIR)
