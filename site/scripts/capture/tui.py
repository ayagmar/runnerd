"""Captures `runnerctl tui` against the demo daemon as the site's frames: runs of text in the TUI's colours.

Reads RUNNERD_CAPTURE_SOCKET and RUNNERD_CAPTURE_HOME from capture.sh; writes site/src/assets/tui/*.json.
"""
import fcntl
import json
import os
import pty
import select
import struct
import termios
import time

import pyte

SOCKET = os.environ["RUNNERD_CAPTURE_SOCKET"]
HOME = os.environ["RUNNERD_CAPTURE_HOME"]
OUT = "site/src/assets/tui"
# The TUI's xterm-256 colours as pyte names them, and the classes site/src/components/Tui.astro styles.
CLASSES = {
    "default": "",
    "7f7f7f": "dim",
    "d7ffd7": "p0",
    "ffd7d7": "p1",
    "87d787": "mem",
    "afafff": "cpu",
    "ffd75f": "wait",
    "ff0000": "bad",
}
# name, columns, rows, keys pressed first (a key is a string; "wait:N" waits N seconds)
FRAMES = [
    ("overview", 136, 28, []),
    ("overview-96", 96, 26, []),
    ("job", 120, 36, ["\r", "wait:2"]),
]


def capture(name, cols, rows, keys):
    pid, fd = pty.fork()
    if pid == 0:
        env = dict(os.environ, TERM="xterm-256color", HOME=HOME)
        env.pop("NO_COLOR", None)
        os.execve("target/debug/runnerctl", ["runnerctl", "--socket", SOCKET, "tui"], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)

    def pump(seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if ready:
                try:
                    stream.feed(os.read(fd, 65536))
                except OSError:
                    return

    pump(3)
    for key in keys:
        if key.startswith("wait:"):
            pump(float(key[5:]))
        else:
            os.write(fd, key.encode())
            pump(1.2)
    lines = []
    for y in range(rows):
        runs = []
        for x in range(cols):
            cell = screen.buffer[y][x]
            classes = [CLASSES.get(cell.fg, "")]
            if cell.bold:
                classes.append("b")
            if cell.reverse:
                classes.append("rv")
            cls = " ".join(c for c in classes if c)
            if cell.data == " " and not cell.reverse and runs and "rv" not in runs[-1][1]:
                cls = runs[-1][1]
            if runs and runs[-1][1] == cls:
                runs[-1][0] += cell.data
            else:
                runs.append([cell.data, cls])
        if runs:
            runs[-1][0] = runs[-1][0].rstrip()
        lines.append([run if run[1] else run[0] for run in runs if run[0]])
    while lines and not lines[-1]:
        lines.pop()
    os.write(fd, b"q")
    pump(0.5)
    with open(f"{OUT}/{name}.json", "w") as out:
        json.dump({"cols": cols, "rows": rows, "lines": lines}, out, ensure_ascii=False, separators=(",", ":"))


for frame in FRAMES:
    capture(*frame)
