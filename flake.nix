{
  description = "OpenWave - Linux control app for the Elgato Wave XLR";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          pythonEnv = pkgs.python3.withPackages (ps: [ ps.pygobject3 ]);
          sitePkgs = pkgs.python3.sitePackages; # "lib/python3.X/site-packages"
          usbLibs = pkgs.lib.makeLibraryPath [ pkgs.libusb1 ];
          runtimeBins = pkgs.lib.makeBinPath [
            pkgs.alsa-utils
            pkgs.pipewire
            pkgs.wireplumber
            pkgs.pulseaudio
          ];
        in
        rec {
          openwave = pkgs.stdenv.mkDerivation {
            pname = "openwave";
            version = pkgs.lib.removeSuffix "\n" (builtins.readFile ./VERSION);
            src = self;

            nativeBuildInputs = with pkgs; [
              makeWrapper
              wrapGAppsHook4
              pythonEnv
              gobject-introspection
            ];
            buildInputs = with pkgs; [
              gtk4
              libadwaita
            ];

            dontBuild = true;
            # Keep the module tree under this output and use the interpreter
            # carrying PyGObject; both launchers retain runtime tool paths.
            installFlags = [
              "PREFIX=${placeholder "out"}"
              "SITEPKG=${placeholder "out"}/${sitePkgs}"
              "PYTHON=${pythonEnv}/bin/python3"
              "INSTALL_METHOD=nix"
            ];

            # Declarative version of the rule wavexlr/setup.py writes on
            # first run -- consume via services.udev.packages on NixOS
            # and the in-app permission check passes out of the box.
            #
            # Generate from setup.py's UDEV_RULES rather than restating them,
            # because udev_installed() requires every supported product ID.
            # Importing the installed module also supports profile-derived rules
            # without maintaining a second literal list for the Nix package.
            postInstall = ''
              mkdir -p $out/lib/udev/rules.d
              PYTHONPATH="$out/${sitePkgs}" ${pythonEnv}/bin/python3 -c \
                'from wavexlr.setup import UDEV_RULES; print(*UDEV_RULES, sep="\n")' \
                > $out/lib/udev/rules.d/99-openwave.rules
            '';

            # ctypes needs to find libusb; the module tree needs to be on
            # PYTHONPATH since it lives in $out, not inside the python env.
            dontWrapGApps = true;
            preFixup = ''
              wrapProgram $out/bin/openwave \
                --prefix PYTHONPATH : $out/${sitePkgs} \
                --prefix LD_LIBRARY_PATH : ${usbLibs} \
                --prefix PATH : ${runtimeBins} \
                --prefix LADSPA_PATH : ${pkgs.ladspaPlugins}/lib/ladspa \
                --prefix XDG_DATA_DIRS : ${pkgs.adwaita-icon-theme}/share \
                "''${gappsWrapperArgs[@]}"

              # Every CLI needs the private module tree, libusb and worker tools.
              for launcher in openwave-daemon openwave-diag openwave-probe; do
                wrapProgram $out/bin/$launcher \
                  --prefix PYTHONPATH : $out/${sitePkgs} \
                  --prefix LD_LIBRARY_PATH : ${usbLibs} \
                  --prefix PATH : ${runtimeBins} \
                  --prefix LADSPA_PATH : ${pkgs.ladspaPlugins}/lib/ladspa
              done
            '';

            meta = {
              description = "Linux control application for the Elgato Wave XLR interface";
              homepage = "https://github.com/rikkichy/openwave";
              license = pkgs.lib.licenses.mit;
              mainProgram = "openwave";
              platforms = pkgs.lib.platforms.linux;
            };
          };
          default = openwave;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          python = pkgs.python3.withPackages (ps: [
            ps.pygobject3
            ps.debugpy
          ]);
          libraries = with pkgs; [
            gtk4
            libadwaita
            libusb1
            pipewire
          ];
          typelibPackages =
            libraries
            ++ (with pkgs; [
              glib
              gdk-pixbuf
              pango
              graphene
              gobject-introspection
            ]);
        in
        {
          default = pkgs.mkShell {
            name = "openwave-dev";
            packages = with pkgs; [
              rustc
              cargo
              rust-analyzer
              rustfmt
              clippy
              llvmPackages.clang
              llvmPackages.clang-tools
              llvmPackages.lld
              llvmPackages.lldb
              gdb
              pkg-config
              gobject-introspection
              cmake
              meson
              ninja
              gnumake
              python
              pyright
              ruff
              nixd
              nixfmt
              shellcheck
              actionlint
              sccache
              pipewire
              wireplumber
              pulseaudio
              alsa-utils
            ];
            buildInputs = libraries;
            OPENWAVE_DEV_SHELL = "1";
            RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            RUST_BACKTRACE = "1";
            shellHook = ''
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath libraries}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              export GI_TYPELIB_PATH="${pkgs.lib.makeSearchPath "lib/girepository-1.0" (map pkgs.lib.getLib typelibPackages)}''${GI_TYPELIB_PATH:+:$GI_TYPELIB_PATH}"
              export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
              export LADSPA_PATH="${pkgs.ladspaPlugins}/lib/ladspa''${LADSPA_PATH:+:$LADSPA_PATH}"
            '';
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
