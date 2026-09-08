{
  description = "OpenWave - Linux control app for the Elgato Wave XLR";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
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
                --prefix LADSPA_PATH : ${pkgs.swh-plugins}/lib/ladspa \
                --prefix XDG_DATA_DIRS : ${pkgs.adwaita-icon-theme}/share \
                "''${gappsWrapperArgs[@]}"

              # The Makefile installs this launcher too; it just needs the same
              # import path as the GUI one. service.py points ExecStart at it.
              wrapProgram $out/bin/openwave-daemon \
                --prefix PYTHONPATH : $out/${sitePkgs} \
                --prefix LD_LIBRARY_PATH : ${usbLibs} \
                --prefix PATH : ${runtimeBins} \
                --prefix LADSPA_PATH : ${pkgs.swh-plugins}/lib/ladspa
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
    };
}
