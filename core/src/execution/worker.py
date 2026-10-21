"""Trusted, embedded launcher. Generated source arrives only after readiness."""

import ctypes
import errno
import importlib
import json
import os
import platform
import resource
import select
import sys
import threading

def keeper(policy):
    # PID 1 has a different identity. Its sole input is a renewable lease.
    identity = policy["keeper"]
    if (os.getuid(), os.getgid(), os.getcwd()) != (
            identity["uid"], identity["gid"], identity["directory"]):
        os._exit(70)
    ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE
    while select.select([0], [], [], policy["lease_seconds"])[0]:
        heartbeat = os.read(0, 64)
        if not heartbeat or heartbeat.strip(b"H"):
            break
    os._exit(0)


def counters(path):
    with open("/sys/fs/cgroup/" + path, encoding="ascii") as stream:
        return dict(line.split() for line in stream)


def controls(policy):
    identity = policy["worker"]
    if (os.getuid(), os.getgid(), os.getcwd()) != (
            identity["uid"], identity["gid"], identity["directory"]):
        raise RuntimeError("identity")
    with open("/proc/self/status", encoding="ascii") as stream:
        status = dict(line.strip().split(":", 1) for line in stream)
    for key, value in [("NoNewPrivs", 1), ("Seccomp", 2), ("CapEff", 0),
                       ("CapPrm", 0), ("CapBnd", 0)]:
        if int(status[key].strip(), 16 if key.startswith("Cap") else 10) != value:
            raise RuntimeError("process controls")
    for name, expected in [("memory.max", policy["memory_bytes"]),
                           ("memory.swap.max", policy["swap_bytes"]),
                           ("pids.max", policy["pids"])]:
        with open("/sys/fs/cgroup/" + name, encoding="ascii") as stream:
            if stream.read().strip() != str(expected):
                raise RuntimeError("cgroup limit")
    with open("/sys/fs/cgroup/cpu.max", encoding="ascii") as stream:
        quota, period = stream.read().split()
        if int(quota) != policy["cpus"] * int(period) or int(period) <= 0:
            raise RuntimeError("CPU limit")
    if not os.statvfs("/").f_flag & os.ST_RDONLY:
        raise RuntimeError("root filesystem")
    required_flags = os.ST_NODEV | os.ST_NOSUID | os.ST_NOEXEC
    for mount in policy["mounts"]:
        stat = os.statvfs(mount["path"])
        if (stat.f_blocks * stat.f_frsize != mount["bytes"]
                or stat.f_files != mount["inodes"]
                or stat.f_flag & required_flags != required_flags):
            raise RuntimeError("temporary filesystem limits")
        if stat.f_bavail == 0 or stat.f_favail == 0:
            raise RuntimeError("temporary filesystem exhausted")
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    return {"memory": counters("memory.events"), "pids": counters("pids.events")}


def restrict_syscalls():
    # Classic BPF operates on struct seccomp_data. Offsets: nr=0, arch=4,
    # args[0]=16. Reject other ABIs before examining syscall numbers.
    if platform.machine() == "x86_64":
        arch, seccomp, clone, clone3, tgkill = 0xC000003E, 317, 56, 435, 234
        denied = [57, 58, 59, 322, 101, 310, 311, 312, 272, 308, 62, 200,
                  129, 297, 298, 304, 321, 323]
    elif platform.machine() == "aarch64":
        arch, seccomp, clone, clone3, tgkill = 0xC00000B7, 277, 220, 435, 131
        denied = [221, 281, 117, 270, 271, 272, 97, 268, 129, 130,
                  138, 240, 241, 265, 280, 282]
    else:
        raise RuntimeError("architecture")
    # pidfd, io_uring and remote-memory interfaces bypass ordinary process APIs.
    denied += [424, 425, 426, 427, 434, 438, 440, 448]
    allow, kill, deny = 0x7FFF0000, 0x80000000, 0x00050000 | errno.EPERM
    instructions = []

    def emit(code, value, yes=0, no=0):
        instructions.append((code, yes, no, value))

    emit(0x20, 4)  # LD arch
    emit(0x15, arch, 1)
    emit(0x06, kill)
    emit(0x20, 0)  # LD nr
    emit(0x45, 0x40000000, 0, 1)  # x32 ABI
    emit(0x06, kill)
    emit(0x15, clone3, 0, 1)
    emit(0x06, 0x00050000 | errno.ENOSYS)  # glibc must use reviewed clone fallback
    for syscall in denied:
        emit(0x15, syscall, 0, 1)
        emit(0x06, deny)
    emit(0x15, clone, 0, 7)
    emit(0x20, 20)  # Reject high flag bits too.
    emit(0x15, 0, 1)
    emit(0x06, deny)
    emit(0x20, 16)
    # glibc pthread flags: VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|
    # PARENT_SETTID|CHILD_CLEARTID. No exit signal or namespace flags.
    emit(0x15, 0x003D0F00, 1)
    emit(0x06, deny)
    emit(0x06, allow)
    emit(0x15, tgkill, 0, 4)
    emit(0x20, 16)
    emit(0x15, os.getpid(), 1)
    emit(0x06, deny)
    emit(0x06, allow)
    emit(0x06, allow)

    class Filter(ctypes.Structure):
        _fields_ = [("code", ctypes.c_ushort), ("jt", ctypes.c_ubyte),
                    ("jf", ctypes.c_ubyte), ("k", ctypes.c_uint32)]

    class Program(ctypes.Structure):
        _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.POINTER(Filter))]

    filters = (Filter * len(instructions))(*(Filter(*entry) for entry in instructions))
    program = Program(len(filters), filters)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
        raise RuntimeError("no new privileges")
    # SECCOMP_SET_MODE_FILTER=1, SECCOMP_FILTER_FLAG_TSYNC=1.
    # A positive return is a failing thread ID, not success.
    if libc.syscall(seccomp, 1, 1, ctypes.byref(program)) != 0:
        raise RuntimeError("seccomp")
    if libc.syscall(clone3, 0, 0) != -1 or ctypes.get_errno() != errno.ENOSYS:
        raise RuntimeError("clone3 fallback")
    completed = []
    thread = threading.Thread(target=lambda: completed.append(True))
    thread.start()
    thread.join()
    if completed != [True]:
        raise RuntimeError("thread fallback")


def worker(policy):
    try:
        observed = controls(policy)
        for module in policy["modules"]:
            importlib.import_module(module)
        restrict_syscalls()
        ready = {"profile": {"python_version": platform.python_version(),
                             "modules": policy["modules"]},
                 "counters": observed}
        print("TK_READY " + json.dumps(ready, separators=(",", ":")), flush=True)
    except BaseException:
        # Preparation errors never include daemon data, source or host paths.
        os._exit(70)

    try:
        source = sys.stdin.buffer.read(policy["source_bytes"] + 1)
        if len(source) > policy["source_bytes"]:
            os._exit(70)
        exec(compile(source, "<code>", "exec"), {"__name__": "__main__"})
    except BaseException as error:
        print(f"{type(error).__name__}: {error}", file=sys.stderr, flush=True)
        if isinstance(error, MemoryError) or (isinstance(error, OSError) and error.errno in
                                             (errno.ENOSPC, errno.ENOMEM, errno.EAGAIN,
                                              errno.EMFILE, errno.ENFILE)):
            sys.exit(70)
        sys.exit(1)


# Core supplies this argument. Standard input remains reserved for source or heartbeats.
try:
    if len(sys.argv) != 3:
        raise ValueError("launcher arguments")
    policy = json.loads(sys.argv[2])
except BaseException:
    os._exit(70)

if sys.argv[1] == "keeper":
    keeper(policy)
elif sys.argv[1] == "worker":
    worker(policy)
else:
    os._exit(70)
