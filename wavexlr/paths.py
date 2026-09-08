"""Locating files that were installed alongside this package.

Everything here is resolved relative to ``__file__`` rather than assuming a
particular prefix. The Makefile honours PREFIX, so hardcoding /usr and
/usr/local only works for two of the possible installs and silently fails for
the rest -- a store path, a --prefix=$HOME/.local install, a virtualenv, or a
staged DESTDIR tree. The failure mode is bad, too: setup.py reports "source not
found" and aborts the rest of first-run setup.

Layout the Makefile produces for a given PREFIX:

    <prefix>/bin/openwave
    <prefix>/bin/openwave-daemon
    <prefix>/<sitepackages>/wavexlr/*.py      <- __file__ is in here
    <prefix>/share/openwave/wireplumber/...
    <prefix>/share/openwave/pipewire/...

<sitepackages> is not a fixed depth (lib/python3.13/site-packages,
lib64/python3.13/site-packages, ...), so walk up until the expected
subdirectory appears instead of counting levels.
"""

import os

_MODULE_DIR = os.path.dirname(os.path.abspath(__file__))
_MAX_DEPTH = 8


def _ancestors():
    """This package's directory and its parents, nearest first."""
    d = _MODULE_DIR
    for _ in range(_MAX_DEPTH):
        yield d
        parent = os.path.dirname(d)
        if parent == d:
            return
        d = parent


def _recorded_prefix():
    from .installation import data_prefix
    try:
        prefix = data_prefix(_MODULE_DIR)
    except (OSError, ValueError, TypeError):
        return None
    return str(prefix) if prefix is not None else None


def data_file(*parts):
    """Return an installed data file's path, or None if it is not present.

    Checked in order: a source checkout, where the data directories sit beside
    the package rather than under share/; then <prefix>/share/openwave for
    every plausible prefix above this module.
    """
    rel = os.path.join(*parts)

    candidates = [os.path.join(os.path.dirname(_MODULE_DIR), rel)]
    prefix = _recorded_prefix()
    if prefix is not None:
        candidates.append(os.path.join(prefix, "share", "openwave", rel))
    candidates += [
        os.path.join(d, "share", "openwave", rel) for d in _ancestors()
    ]

    for candidate in candidates:
        if os.path.exists(candidate):
            return candidate
    return None


def bin_file(name):
    """Return the path to one of our installed launchers, or None.

    Prefers the copy under this package's own prefix so a service unit keeps
    pointing at the install it was generated from, rather than whichever one
    happens to be first on PATH later.
    """
    prefix = _recorded_prefix()
    candidates = ([prefix] if prefix is not None else []) + list(_ancestors())
    for d in candidates:
        candidate = os.path.join(d, "bin", name)
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    return None
