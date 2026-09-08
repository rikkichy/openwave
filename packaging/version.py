"""Print the sole release version, optionally checking a supplied tag."""
import argparse
from pathlib import Path
import re

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--tag", help="Require this tag to equal v<VERSION>")
args = parser.parse_args()
raw = (Path(__file__).resolve().parents[1] / "VERSION").read_text()
version = raw.removesuffix("\n")
if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version):
    parser.error("VERSION must contain exactly one stable MAJOR.MINOR.PATCH version")
if args.tag is not None and args.tag != f"v{version}":
    parser.error(f"tag {args.tag!r} does not match VERSION ({version})")
print(version)
