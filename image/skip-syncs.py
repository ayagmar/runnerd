#!/usr/bin/python3 -I
"""Runs its arguments with fsync and the other sync calls answered 0 without syncing, when runnerd asks for it.

Under the runner's entrypoint, before anything else of the runner's starts: with RUNNERD_SKIP_SYNCS=1, which runnerd
sets by default ([host] skip_syncs, docs/adr/0141), it installs a seccomp filter in which fsync, fdatasync, sync,
syncfs, sync_file_range and msync return 0 at once, then execs its arguments. The filter cannot be removed and every
process started from here on inherits it: the runner, its job, the container's Docker Engine and its containers and
builders, whose own filters the kernel runs beside this one, with an error winning over an allow. Everything a runner
writes is a cache or goes with its container (ADR 0054 empties a slot's Docker store that does not start), so a sync
spares the disk nothing it needs. Without the variable, or where the filter cannot be installed, it only execs its
arguments: the runner then syncs as before.

Docker's own seccomp profile cannot do this: sysbox-runc removes a profile's error rule for a call its own allowlist
holds, which every sync call is. This is a filter of its own, which root in the container may install since it has
CAP_SYS_ADMIN in the container's user namespace, so no_new_privs, which would stop the runner's sudo, stays unset.

The numbers are from the kernel's syscall tables (arch/x86/entry/syscalls/syscall_64.tbl and
include/uapi/asm-generic/unistd.h): x86_64 msync 26, fsync 74, fdatasync 75, sync 162, sync_file_range 277,
syncfs 306; aarch64 sync 81, fsync 82, fdatasync 83, sync_file_range 84, msync 227, syncfs 267. A call of another
ABI, a 32-bit program on x86_64, syncs as before.
"""

import ctypes
import os
import struct
import sys

# The AUDIT_ARCH_* value seccomp reports for each machine, and its sync calls' numbers.
ARCHES = {
    "x86_64": (0xC000003E, (26, 74, 75, 162, 277, 306)),
    "aarch64": (0xC00000B7, (81, 82, 83, 84, 227, 267)),
}

BPF_LD_W_ABS = 0x20
BPF_JMP_JEQ_K = 0x15
BPF_RET_K = 0x06
SECCOMP_RET_ALLOW = 0x7FFF0000
SECCOMP_RET_ERRNO = 0x00050000
# Offsets in struct seccomp_data.
NR, ARCH = 0, 4
PR_SET_SECCOMP, SECCOMP_MODE_FILTER = 22, 2


class SockFprog(ctypes.Structure):
    _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]


def program(arch, numbers):
    """The filter: another ABI's call is allowed, one of `numbers` returns 0, every other call is allowed."""
    count = len(numbers)
    instructions = [
        (BPF_LD_W_ABS, 0, 0, ARCH),
        (BPF_JMP_JEQ_K, 0, count + 1, arch),
        (BPF_LD_W_ABS, 0, 0, NR),
    ]
    for index, number in enumerate(numbers):
        instructions.append((BPF_JMP_JEQ_K, count - index, 0, number))
    instructions.append((BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW))
    # Errno 0: the call returns 0.
    instructions.append((BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO))
    return instructions


def skip_syncs():
    """Installs the filter; None once it is in, else why it is not."""
    machine = os.uname().machine
    if machine not in ARCHES:
        return f"no sync calls known for {machine}"
    code = b"".join(struct.pack("HBBI", *instruction) for instruction in program(*ARCHES[machine]))
    buffer = ctypes.create_string_buffer(code)
    fprog = SockFprog(len(code) // 8, ctypes.addressof(buffer))
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, ctypes.byref(fprog), 0, 0) != 0:
        return os.strerror(ctypes.get_errno())
    return None


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: runnerd-skip-syncs COMMAND [ARGUMENT...]")
    if os.environ.get("RUNNERD_SKIP_SYNCS") == "1":
        refused = skip_syncs()
        if refused is not None:
            print(f"runnerd: cannot skip sync calls ({refused}): this runner syncs", file=sys.stderr)
    os.execvp(sys.argv[1], sys.argv[1:])


if __name__ == "__main__":
    main()
