PREFIX ?= /usr/local
DESTDIR ?=
INSTALL_METHOD ?= manual
CARGO ?= cargo
RUSTC ?= rustc
CARGO_TARGET_DIR ?= target
BINARY_DIR ?= $(CARGO_TARGET_DIR)/release
CARGO_BUILD_FLAGS ?= --release --locked --workspace --bins
DESTDIR_ARGS = $(if $(strip $(DESTDIR)),--destdir "$(DESTDIR)")

BINDIR = $(DESTDIR)$(PREFIX)/bin
LIBEXECDIR = $(DESTDIR)$(PREFIX)/libexec
DATADIR = $(DESTDIR)$(PREFIX)/share
APPDIR = $(DATADIR)/openwave
DESKTOPDIR = $(DATADIR)/applications
DOCDIR = $(DATADIR)/doc/openwave
LICENSEDIR = $(DATADIR)/licenses/openwave
PUBLIC_BINARIES = openwave openwave-daemon openwave-diag openwave-probe
BINARIES = $(PUBLIC_BINARIES) openwave-maintenance
DOCS = ARCHITECTURE.md hardware-support.md install-bazzite.md protocol.md troubleshooting.md
ASSETS = wavexlr.desktop openwave-autostart.desktop VERSION data/style.css \
	wireplumber/51-openwave-wave-xlr.conf pipewire/52-openwave-mixes.conf \
	com.github.openwave.metainfo.xml icons/openwave.svg icons/openwave-white.svg \
	icons/openwave-black.svg icons/openwave-red.svg README.md \
	packaging/asset-attribution.txt LICENSE $(addprefix docs/,$(DOCS))

.PHONY: all build check-toolchain check-install-context check-binaries check-version check-payload install uninstall
all: build

check-toolchain:
	@test "$$(id -u)" != 0 || { echo 'Build as an ordinary user; use install.sh as the login user for a privileged manual installation.' >&2; exit 1; }
	@set -- $$($(RUSTC) --version); test "$$1 $$2" = 'rustc 1.98.1' || { echo 'OpenWave requires Rust 1.98.1.' >&2; exit 1; }

build: check-toolchain
	CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(CARGO) build $(CARGO_BUILD_FLAGS)

# Reject live root manual installation before any build or payload execution.
# Staging and package-manager builds retain their ordinary build-user workflow.
check-install-context:
	@if test "$(INSTALL_METHOD)" = manual && test -z "$(DESTDIR)" && test "$$(id -u)" = 0; then \
		echo 'Do not run sudo make install. Run install.sh as your login user: it stages and checks the payload before authorizing a trusted root-owned install-payload helper. For a user prefix, run make install PREFIX="$$HOME/.local" without sudo.' >&2; \
		exit 1; \
	fi

check-binaries: check-install-context
	@missing=0; for binary in $(BINARIES); do test -x "$(BINARY_DIR)/$$binary" || missing=1; done; \
	if test "$$missing" = 1; then $(MAKE) build; fi
	@set -e; for binary in $(BINARIES); do \
		test -f "$(BINARY_DIR)/$$binary" && test ! -L "$(BINARY_DIR)/$$binary" && test -x "$(BINARY_DIR)/$$binary" || { echo "Missing regular executable: $(BINARY_DIR)/$$binary" >&2; exit 1; }; \
		test "$$(od -An -tx1 -N4 "$(BINARY_DIR)/$$binary" | tr -d ' \n')" = 7f454c46 || { echo "Not a native ELF binary: $$binary" >&2; exit 1; }; \
	done

check-version: check-binaries
	@set -e; version=$$("$(BINARY_DIR)/openwave-maintenance" version --file VERSION); \
	for binary in $(BINARIES); do \
		reported=$$("$(BINARY_DIR)/$$binary" --version); \
		test "$$reported" = "$$binary $$version" || { echo "Compiled version mismatch: $$binary (expected $$version, got $$reported)" >&2; exit 1; }; \
	done

check-payload: check-version
	@set -e; for asset in $(ASSETS); do \
		test -f "$$asset" && test ! -L "$$asset" || { echo "Missing regular payload file: $$asset" >&2; exit 1; }; \
	done

install: check-payload
	# This check MUST precede every destination creation or overwrite.
	"$(BINARY_DIR)/openwave-maintenance" record-install --check --prefix "$(PREFIX)" $(DESTDIR_ARGS) --method "$(INSTALL_METHOD)"
	install -dm755 "$(BINDIR)" "$(LIBEXECDIR)"
	@set -e; for binary in $(PUBLIC_BINARIES); do install -m755 "$(BINARY_DIR)/$$binary" "$(BINDIR)/$$binary"; done
	install -m755 "$(BINARY_DIR)/openwave-maintenance" "$(LIBEXECDIR)/openwave-maintenance"
	install -Dm644 wavexlr.desktop "$(DESKTOPDIR)/openwave.desktop"
	install -Dm644 openwave-autostart.desktop "$(APPDIR)/openwave-autostart.desktop"
	install -Dm644 wireplumber/51-openwave-wave-xlr.conf "$(APPDIR)/wireplumber/51-openwave-wave-xlr.conf"
	install -Dm644 pipewire/52-openwave-mixes.conf "$(APPDIR)/pipewire/52-openwave-mixes.conf"
	install -Dm644 VERSION "$(APPDIR)/VERSION"
	install -Dm644 data/style.css "$(APPDIR)/style.css"
	install -Dm644 com.github.openwave.metainfo.xml "$(DATADIR)/metainfo/com.github.openwave.metainfo.xml"
	install -Dm644 icons/openwave.svg "$(DATADIR)/icons/hicolor/scalable/apps/openwave.svg"
	install -dm755 "$(DATADIR)/icons/hicolor/scalable/status" "$(APPDIR)/icons"
	install -m644 icons/openwave-white.svg icons/openwave-black.svg icons/openwave-red.svg "$(DATADIR)/icons/hicolor/scalable/status/"
	install -m644 icons/openwave.svg icons/openwave-white.svg icons/openwave-black.svg icons/openwave-red.svg "$(APPDIR)/icons/"
	install -Dm644 README.md "$(DOCDIR)/README.md"
	install -Dm644 icons/openwave.svg "$(DOCDIR)/icons/openwave.svg"
	@set -e; for doc in $(DOCS); do install -Dm644 "docs/$$doc" "$(DOCDIR)/docs/$$doc"; done
	install -Dm644 packaging/asset-attribution.txt "$(DOCDIR)/asset-attribution.txt"
	install -Dm644 LICENSE "$(LICENSEDIR)/LICENSE"
	"$(BINARY_DIR)/openwave-maintenance" record-install --prefix "$(PREFIX)" $(DESTDIR_ARGS) --method "$(INSTALL_METHOD)"

# Explicit application-files-only removal; DESTDIR never addresses live services.
uninstall:
	@helper="$(BINARY_DIR)/openwave-maintenance"; \
	if test ! -x "$$helper"; then helper="$(LIBEXECDIR)/openwave-maintenance"; fi; \
	test -x "$$helper" || { echo 'A built or installed native maintenance binary is required.' >&2; exit 1; }; \
	"$$helper" files-only --prefix "$(PREFIX)" $(DESTDIR_ARGS) --yes
