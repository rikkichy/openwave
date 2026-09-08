"""Informational and uninstall options, before GTK or USB startup."""

import argparse
from pathlib import Path

from . import paths


def main():
    parser = argparse.ArgumentParser(prog="openwave", description="Wave hardware controls and PipeWire audio mixes")
    parser.add_argument("--hide", action="store_true", help="Start hidden when a system tray host is available")
    parser.add_argument("--version", action="store_true", help="Print the installed version without opening audio or USB")
    parser.add_argument("--uninstall", action="store_true", help="Remove this installation or show package-manager removal guidance")
    parser.add_argument("--delete-settings", action="store_true", help="With --uninstall, also delete settings and saved scenes")
    parser.add_argument("--yes", action="store_true", help="With --uninstall, confirm removal without an interactive prompt")
    parser.add_argument("--dry-run", action="store_true", help="With --uninstall, inspect removal without making changes")
    options, remaining = parser.parse_known_args()
    if options.uninstall:
        if options.hide or options.version or remaining:
            parser.error("--uninstall cannot be combined with GUI or informational options")
        from .uninstall import main as uninstall
        flags = [flag for flag, enabled in (
            ("--delete-settings", options.delete_settings),
            ("--yes", options.yes), ("--dry-run", options.dry_run),
        ) if enabled]
        return uninstall(flags)
    if options.delete_settings or options.yes or options.dry_run:
        parser.error("--delete-settings, --yes and --dry-run require --uninstall")
    if options.version:
        version_file = paths.data_file("VERSION")
        if version_file is None:
            parser.error("Installed VERSION file is missing")
        print("OpenWave " + Path(version_file).read_text().strip())
        return 0
    from .app import main as run
    return run()


if __name__ == "__main__":
    raise SystemExit(main())
