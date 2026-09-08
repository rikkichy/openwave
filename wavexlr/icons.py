"""Resolve icons at draw time without changing persisted source/mix choices."""

from functools import lru_cache
import logging
import os

from . import paths

TRAY_ICON_COLORS = ("white", "black")

# Import GTK only when a display is available; icon metadata is also used headless.
_ALTERNATIVES = {
    "web-browser-symbolic": (
        "internet-web-browser-symbolic",
        "applications-internet-symbolic",
        "globe-symbolic",
    ),
    "input-gaming-symbolic": (
        "applications-games-symbolic",
        "input-gamepad-symbolic",
    ),
    "audio-x-generic-symbolic": (
        "multimedia-player-symbolic",
        "media-optical-audio-symbolic",
    ),
    "list-drag-handle-symbolic": (
        "view-list-symbolic",
        "open-menu-symbolic",
    ),
    "network-transmit-symbolic": (
        "network-wired-symbolic",
        "network-connect-symbolic",
    ),
    "preferences-desktop-multimedia-symbolic": (
        "multimedia-player-symbolic",
        "applications-multimedia-symbolic",
    ),
    "video-display-symbolic": (
        "computer-symbolic",
        "preferences-desktop-display-symbolic",
    ),
}

_cache = {}
_watched = False


def _theme():
    """The display's icon theme, or None when there is no display yet."""
    global _watched
    try:
        import gi
        gi.require_version("Gtk", "4.0")
        from gi.repository import Gdk, Gtk
    except (ImportError, ValueError):
        return None
    display = Gdk.Display.get_default()
    if display is None:
        return None
    theme = Gtk.IconTheme.get_for_display(display)
    if theme is not None and not _watched:
        # A theme change makes every earlier answer stale, including the ones
        # that needed no substitution.
        theme.connect("changed", lambda *_: _cache.clear())
        _watched = True
    return theme


def resolve(name):
    """Return name, or the nearest name the active theme actually has.

    Unknown names are returned untouched: a theme we have no table for is not
    improved by guessing, and the broken glyph is at least honest about it.
    """
    if not name:
        return name
    if name in _cache:
        return _cache[name]

    theme = _theme()
    if theme is None:
        return name
    chosen = name
    if theme is not None and not theme.has_icon(name):
        for alternative in _ALTERNATIVES.get(name, ()):
            if theme.has_icon(alternative):
                chosen = alternative
                break

    _cache[name] = chosen
    return chosen


@lru_cache(maxsize=None)
def _asset_path(name):
    path = paths.data_file("icons", f"{name}.svg")
    if path is None:
        logging.warning("icons: supplied artwork %s.svg is missing", name)
    return path


def theme_path(name):
    """Directory containing supplied artwork, or empty when it is missing."""
    path = _asset_path(name)
    return os.path.dirname(path) if path else ""
