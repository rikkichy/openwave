"""Headless capture keepalive with observation-only health checks by default."""

import argparse
import logging
import signal
import threading

from .paths import data_file

log = logging.getLogger("openwave.daemon")


def _version():
    path = data_file("VERSION")
    if path is not None:
        try:
            with open(path) as stream:
                return stream.read().strip()
        except OSError:
            pass
    return "unknown"


def main(argv=None):
    parser = argparse.ArgumentParser(prog="openwave-daemon", description=__doc__)
    parser.add_argument("--version", action="version", version=f"OpenWave {_version()}")
    parser.add_argument("--auto-recover", action="store_true",
                        help="allow bounded card-profile cycles and sink suspend/resume on confirmed faults")
    args = parser.parse_args(argv)

    # --help and --version must work without audio, USB or GTK dependencies.
    from .audio import AudioManager
    from .health import HealthMonitor

    logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")
    stopped = threading.Event()

    def shutdown(sig, frame):
        stopped.set()

    def on_status(present, healthy, state):
        if not present:
            log.info("Device not detected")
        elif state == "silent":
            log.error("Capture delivers digital silence; power-cycle the device")
        elif healthy:
            log.info("Capture keepalive active")
        else:
            log.warning("Establishing capture keepalive...")

    manager = AudioManager(on_status_change=on_status)
    health = HealthMonitor(auto_recover=args.auto_recover)
    previous = {sig: signal.signal(sig, shutdown)
                for sig in (signal.SIGTERM, signal.SIGINT)}

    try:
        log.info("Starting OpenWave audio daemon (auto-recover: %s)", args.auto_recover)
        manager.start()
        health.start()
        stopped.wait()
    finally:
        try:
            # Finish any owed restoration before tearing down capture pins.
            health.stop()
        finally:
            try:
                manager.stop()
            finally:
                for sig, handler in previous.items():
                    signal.signal(sig, handler)


if __name__ == "__main__":
    main()
