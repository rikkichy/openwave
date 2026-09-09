{
  description = "OpenWave - Linux control app for the Elgato Wave XLR";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" ];
      environment = system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in {
          inherit pkgs toolchain;
          rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };
          libraries = with pkgs; [ gtk4 libadwaita libusb1 pipewire ];
          runtimeTools = with pkgs; [ alsa-utils pipewire wireplumber pulseaudio ];
          smokeTools = with pkgs; [
            bubblewrap xorg-server xauth dbus xdotool imagemagick
            flatpak flatpak-builder ostree podman jq curl git
            desktop-file-utils appstream at-spi2-core util-linux procps
          ];
        };
      # Explicit source roots also protect path:. builds (which include ignored
      # files). Local guidance, credentials, build trees and caches never enter
      # either the Cargo vendor derivation or the application derivation.
      source = nixpkgs.lib.cleanSourceWith {
        src = ./.;
        filter = path: type:
          let
            lib = nixpkgs.lib;
            relative = lib.removePrefix (toString ./. + "/") (toString path);
            parts = lib.splitString "/" relative;
            root = builtins.head parts;
            name = baseNameOf path;
            roots = [ "crates" "data" "docs" "icons" "packaging" "pipewire" "wireplumber" ];
            files = [
              "Cargo.toml" "Cargo.lock" "rust-toolchain.toml" "VERSION" "Makefile"
              "PKGBUILD" "LICENSE" "README.md" "wavexlr.desktop"
              "openwave-autostart.desktop" "com.github.openwave.metainfo.xml"
            ];
            excluded = component:
              lib.hasPrefix "." component || builtins.elem component [
                "target" "vendor" "node_modules" "__pycache__" "build" "result"
                "AGENTS.md" "CLAUDE.md"
              ];
          in
            !(lib.any excluded parts)
            && (builtins.elem root roots || builtins.elem relative files)
            && (type == "directory" || type == "regular")
            && lib.cleanSourceFilter path type;
      };
    in {
      packages = forAllSystems (system:
        let
          e = environment system;
          inherit (e) pkgs;
          runtimeBins = pkgs.lib.makeBinPath e.runtimeTools;
        in rec {
          openwave = e.rustPlatform.buildRustPackage {
            pname = "openwave";
            version = pkgs.lib.removeSuffix "\n" (builtins.readFile ./VERSION);
            src = source;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--workspace" "--bins" ];
            cargoTestFlags = [ "--workspace" ];
            nativeBuildInputs = with pkgs; [ pkg-config makeWrapper wrapGAppsHook4 ];
            nativeCheckInputs = with pkgs; [ dbus bubblewrap ];
            buildInputs = e.libraries;
            installPhase = ''
              runHook preInstall
              make install PREFIX="$out" INSTALL_METHOD=nix \
                BINARY_DIR="target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release"
              mkdir -p "$out/lib/udev/rules.d"
              "$out/libexec/openwave-maintenance" udev-rules > "$out/lib/udev/rules.d/99-openwave.rules"
              runHook postInstall
            '';
            # Manager ownership outranks the pre-wrapper receipt hashes.
            dontWrapGApps = true;
            preFixup = ''
              wrapProgram "$out/bin/openwave" \
                --prefix PATH : ${runtimeBins} \
                --prefix LADSPA_PATH : ${pkgs.ladspaPlugins}/lib/ladspa \
                --prefix XDG_DATA_DIRS : ${pkgs.adwaita-icon-theme}/share \
                "''${gappsWrapperArgs[@]}"
              for launcher in openwave-daemon openwave-diag openwave-probe; do
                wrapProgram "$out/bin/$launcher" \
                  --prefix PATH : ${runtimeBins} \
                  --prefix LADSPA_PATH : ${pkgs.ladspaPlugins}/lib/ladspa
              done
            '';
            meta = {
              description = "Linux control application for Elgato Wave devices";
              homepage = "https://github.com/rikkichy/openwave";
              license = pkgs.lib.licenses.mit;
              mainProgram = "openwave";
              platforms = [ "x86_64-linux" ];
            };
          };
          default = openwave;
        });

      checks = forAllSystems (system:
        let
          e = environment system;
          inherit (e) pkgs;
          package = self.packages.${system}.openwave;
        in {
          native = package;
          installed = pkgs.runCommand "openwave-installed-native-proof" {
            nativeBuildInputs = e.smokeTools ++ e.runtimeTools;
          } ''
            export HOME="$TMPDIR/home"
            export XDG_CONFIG_HOME="$HOME/config" XDG_DATA_HOME="$HOME/data"
            export XDG_STATE_HOME="$HOME/state" XDG_RUNTIME_DIR="$HOME/run"
            mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"
            chmod 700 "$HOME" "$XDG_RUNTIME_DIR"
            cd "$TMPDIR"
            for binary in openwave openwave-daemon openwave-diag; do
              ${package}/bin/$binary --help
              expected="$binary"
              test "$(${package}/bin/$binary --version)" = "$expected ${package.version}"
            done
            ${package}/bin/openwave-probe --help
            test -s ${package}/share/openwave/style.css
            test -s ${package}/lib/udev/rules.d/99-openwave.rules
            mkdir -p "$out"
            printf '%s\n' '${system}: built and executed installed native informational paths' > "$out/result"
          '';
        });

      devShells = forAllSystems (system:
        let e = environment system; inherit (e) pkgs toolchain;
        in {
          default = pkgs.mkShell {
            name = "openwave-dev";
            packages = (with pkgs; [
              toolchain llvmPackages.clang llvmPackages.clang-tools llvmPackages.lld
              llvmPackages.lldb gdb pkg-config cmake meson ninja gnumake
              nixd nixfmt shellcheck actionlint sccache
            ]);
            buildInputs = e.libraries;
            OPENWAVE_DEV_SHELL = "1";
            RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            RUST_BACKTRACE = "1";
            shellHook = ''
              export PATH="${pkgs.lib.makeBinPath (e.runtimeTools ++ e.smokeTools)}:$PATH"
              export XDG_DATA_DIRS="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.adwaita-icon-theme}/share:${pkgs.at-spi2-core}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
              export LADSPA_PATH="${pkgs.ladspaPlugins}/lib/ladspa''${LADSPA_PATH:+:$LADSPA_PATH}"
            '';
          };
        });
      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
