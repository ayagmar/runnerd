#!/usr/bin/python3 -I
"""Tries the calls Sysbox serves in a runner container, and exits with what it found (docs/adr/0155).

The entrypoint runs it as root after the runner exited, every process of the `runner` user was killed and the scrub
ran, under `timeout`. It makes the calls the 12 runner faults recorded up to 2026-11-20 failed on: mount and umount
(runc's /dev/shm, /dev/pts, /sys, /proc and bind mounts, containerd's overlays) and openat2 under /proc (runc's
"unsafe procfs detected"). Sysbox traps them through the container's seccomp listener, which every process of the
container inherits from its init, so this one goes through the same listener the job's did.

Its mounts are in a mount namespace of its own, which goes when it exits, on a tmpfs over /mnt there: it changes
nothing in the container's file system, which a failed job's record keeps, even when an unmount fails. Errors go to
stderr, which is the container's log.

Exit codes, which runnerd reads from the stopped container (docker::probe): 200 every call worked, 201 a mount or
an unmount failed, 202 openat2 under /proc failed, 203 both. Any other code, such as a traceback's 1 or a failure
before the first call, is the entrypoint's 205: the probe could not run. A probe that does not end is timeout's 124.
"""

import ctypes
import os
import sys

CLEAN, MOUNT, PROCFS = 200, 1, 2
# Could not run: the entrypoint maps every code but 200 to 203 to 205.
BROKEN = 2

MS_RDONLY, MS_NOSUID, MS_NODEV, MS_NOEXEC = 0x1, 0x2, 0x4, 0x8
MS_BIND, MS_REC, MS_PRIVATE = 0x1000, 0x4000, 0x40000
# openat2 is 437 on every architecture (include/uapi/asm-generic/unistd.h and syscall_64.tbl).
SYS_OPENAT2 = 437
RESOLVE_NO_MAGICLINKS = 0x02
# Where its tmpfs goes, in its own mount namespace: a directory every Ubuntu image has, empty.
SCRATCH = "/mnt"
# The file runc opened when one fault hit (job 133762565028), which Sysbox serves.
PROCFS_FILE = b"/proc/sys/net/ipv4/ip_unprivileged_port_start"


class OpenHow(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_uint64), ("mode", ctypes.c_uint64), ("resolve", ctypes.c_uint64)]


libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_char_p]
libc.umount2.argtypes = [ctypes.c_char_p, ctypes.c_int]
libc.syscall.restype = ctypes.c_long


def failed(call):
    """Prints why `call` failed, with errno's name, and returns True."""
    error = ctypes.get_errno()
    print(f"runnerd-probe: {call}: {os.strerror(error)} ({error})", file=sys.stderr)
    return True


def mount(source, target, fstype, flags=0):
    """Mounts and unmounts `target`; True when either failed."""
    if libc.mount(source, target.encode(), fstype, flags, None) != 0:
        return failed(f"mount {(fstype or b'bind').decode()} on {target}")
    if libc.umount2(target.encode(), 0) != 0:
        return failed(f"umount {target}")
    return False


def mounts(scratch):
    """The mounts runc makes for a container, on a tmpfs over `scratch`; True when one failed."""
    if libc.mount(None, b"/", None, MS_REC | MS_PRIVATE, None) != 0:
        return failed("make / private")
    if libc.mount(b"tmpfs", scratch.encode(), b"tmpfs", MS_NOSUID | MS_NODEV, b"size=64k") != 0:
        return failed(f"mount tmpfs on {scratch}")
    names = ("proc", "sys", "source", "bound")
    for name in names:
        os.mkdir(os.path.join(scratch, name))
    path = {name: os.path.join(scratch, name) for name in names}
    broke = mount(b"proc", path["proc"], b"proc", MS_NOSUID | MS_NODEV | MS_NOEXEC)
    broke |= mount(b"sysfs", path["sys"], b"sysfs", MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC)
    broke |= mount(path["source"].encode(), path["bound"], None, MS_BIND)
    if libc.umount2(scratch.encode(), 0) != 0:
        broke |= failed(f"umount {scratch}")
    return broke


def procfs():
    """Opens and reads a file Sysbox serves under /proc with openat2; True when that failed."""
    how = OpenHow(os.O_RDONLY | os.O_CLOEXEC, 0, RESOLVE_NO_MAGICLINKS)
    fd = libc.syscall(SYS_OPENAT2, -100, PROCFS_FILE, ctypes.byref(how), ctypes.sizeof(how))
    if fd < 0:
        return failed(f"openat2 {PROCFS_FILE.decode()}")
    try:
        os.read(fd, 64)
    except OSError as error:
        print(f"runnerd-probe: read {PROCFS_FILE.decode()}: {error}", file=sys.stderr)
        return True
    finally:
        os.close(fd)
    return False


def main():
    try:
        os.unshare(os.CLONE_NEWNS)
    except OSError as error:
        print(f"runnerd-probe: cannot start: {error}", file=sys.stderr)
        sys.exit(BROKEN)
    found = (MOUNT if mounts(SCRATCH) else 0) | (PROCFS if procfs() else 0)
    sys.exit(CLEAN + found)


if __name__ == "__main__":
    main()
