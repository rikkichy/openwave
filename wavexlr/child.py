"""Exec audio helpers with Linux parent-death protection, without preexec_fn.

The prctl call runs in a fresh interpreter, never between fork and exec in the
threaded GTK process. The helper becomes the audio process; no supervisor stays.
"""

import os
import subprocess
import sys


def spawn(argv, **kwargs):
    return subprocess.Popen(
        [sys.executable, "-m", "wavexlr.child", str(os.getpid()), *argv], **kwargs
    )


def main():
    import ctypes
    import signal

    parent = int(sys.argv[1])
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(1, int(signal.SIGTERM), 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "Cannot protect audio child lifetime")
    if os.getppid() != parent:
        return 1
    os.execvp(sys.argv[2], sys.argv[2:])


if __name__ == "__main__":
    sys.exit(main())
