"""Headless informational options, before GTK or USB imports."""

import argparse
from pathlib import Path

from . import paths


def main():
    parser = argparse.ArgumentParser(prog="openwave", description="Wave hardware controls and PipeWire audio mixes")
    parser.add_argument("--hide", action="store_true", help="Start hidden when a system tray host is available")
    parser.add_argument("--version", action="store_true", help="Print the installed version without opening audio or USB")
    options, _ = parser.parse_known_args()
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
