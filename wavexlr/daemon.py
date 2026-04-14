"""Headless daemon that just runs the audio capture fix."""

import logging
import signal
import time
import sys

from .audio import AudioManager

logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")
log = logging.getLogger("openwave.daemon")


def main():
    log.info("Starting OpenWave audio daemon")

    def on_status(present, healthy):
        if not present:
            log.info("Device not detected")
        elif healthy:
            log.info("Capture keepalive active")
        else:
            log.warning("Establishing capture keepalive...")

    mgr = AudioManager(on_status_change=on_status)
    mgr.start()

    def shutdown(sig, frame):
        log.info("Shutting down")
        mgr.stop()
        sys.exit(0)

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    # Keep main thread alive
    while True:
        time.sleep(3600)
