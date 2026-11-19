use crate::{SandboxConfig, SandboxSpec};
use serde_json::{Value, json};

pub(crate) fn bundle(config: &SandboxConfig, spec: &SandboxSpec) -> Value {
    let dir = crate::staging::directory(spec);
    let uid = spec.uid(config);
    let caps = [
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_SETGID",
        "CAP_SETUID",
        "CAP_SETFCAP",
        "CAP_KILL",
        "CAP_SYS_CHROOT",
        "CAP_NET_BIND_SERVICE",
    ];
    let allow = "read write readv writev pread64 pwrite64 preadv pwritev preadv2 pwritev2 close close_range lseek dup dup2 dup3 fcntl flock ioctl open openat openat2 creat stat lstat fstat newfstatat statx access faccessat faccessat2 readlink readlinkat getdents getdents64 getcwd chdir fchdir chroot mkdir mkdirat rmdir unlink unlinkat rename renameat renameat2 link linkat symlink symlinkat truncate ftruncate fallocate chmod fchmod fchmodat fchmodat2 chown fchown lchown fchownat umask utime utimes futimesat utimensat fsync fdatasync sync syncfs sync_file_range statfs fstatfs getxattr lgetxattr fgetxattr setxattr lsetxattr fsetxattr listxattr llistxattr flistxattr removexattr lremovexattr fremovexattr mmap mmap2 munmap mprotect mremap madvise brk mlock munlock mlockall munlockall mincore msync membarrier poll ppoll select pselect6 epoll_create epoll_create1 epoll_ctl epoll_wait epoll_pwait epoll_pwait2 eventfd eventfd2 pipe pipe2 splice tee vmsplice sendfile copy_file_range socketpair bind connect listen accept accept4 getsockname getpeername shutdown getsockopt setsockopt sendto recvfrom sendmsg recvmsg sendmmsg recvmmsg rt_sigaction rt_sigprocmask rt_sigreturn rt_sigpending rt_sigtimedwait rt_sigsuspend sigaltstack signalfd signalfd4 kill tkill tgkill restart_syscall fork vfork execve execveat exit exit_group wait4 waitid getpid getppid gettid set_tid_address set_robust_list get_robust_list futex futex_waitv rseq prctl arch_prctl getuid geteuid getgid getegid getresuid getresgid getgroups setuid setgid setreuid setregid setresuid setresgid setgroups setfsuid setfsgid capget capset setsid getsid setpgid getpgid getpgrp sched_yield sched_getaffinity sched_setaffinity sched_getparam sched_setparam sched_getscheduler sched_setscheduler sched_get_priority_max sched_get_priority_min sched_rr_get_interval getpriority setpriority getrlimit setrlimit prlimit64 getrusage times sysinfo uname getrandom time gettimeofday clock_gettime clock_getres clock_nanosleep nanosleep alarm pause getitimer setitimer timer_create timer_delete timer_gettime timer_settime timer_getoverrun timerfd_create timerfd_gettime timerfd_settime inotify_init inotify_init1 inotify_add_watch inotify_rm_watch memfd_create fadvise64 readahead";
    json!({
        "ociVersion": "1.1.0",
        "hostname": format!("agent-{}", spec.slot),
        "root": {"path": dir.join("rootfs"), "readonly": false},
        "process": {
            "terminal": false, "user": {"uid": 0, "gid": 0}, "cwd": "/work",
            "args": ["/opt/survive/survive-init"],
            "env": ["PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "HOME=/work/.home", "CARGO_HOME=/work/.cargo", "RUSTUP_HOME=/usr/local/rustup", "TMPDIR=/tmp", "LANG=C.UTF-8", "SURVIVE_WORK_DIR=/work", format!("SURVIVE_ENV_URL={}", config.gateway_url), format!("SURVIVE_TOKEN={}", spec.token)],
            "noNewPrivileges": true,
            "capabilities": {"bounding": caps, "effective": caps, "permitted": caps, "inheritable": [], "ambient": []},
            "rlimits": [{"type":"RLIMIT_NOFILE", "hard":4096, "soft":4096}, {"type":"RLIMIT_CORE", "hard":0, "soft":0}]
        },
        "mounts": [
            {"destination":"/proc", "type":"proc", "source":"proc", "options":["nosuid","nodev","noexec"]},
            {"destination":"/dev", "type":"tmpfs", "source":"tmpfs", "options":["nosuid","strictatime","mode=755","size=16m"]},
            {"destination":"/dev/pts", "type":"devpts", "source":"devpts", "options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]},
            {"destination":"/dev/shm", "type":"tmpfs", "source":"shm", "options":["nosuid","nodev","noexec","mode=1777","size=64m"]},
            {"destination":"/sys", "type":"tmpfs", "source":"tmpfs", "options":["nosuid","nodev","noexec","ro","size=1m"]},
            {"destination":"/tmp", "type":"tmpfs", "source":"tmpfs", "options":["nosuid","nodev","mode=1777","size=256m"]},
            {"destination":"/run", "type":"tmpfs", "source":"tmpfs", "options":["nosuid","nodev","noexec","mode=755","size=16m"]},
            {"destination":"/work", "type":"bind", "source":dir.join("work"), "options":["rbind","rw","nosuid","nodev","rprivate"]},
            {"destination":"/opt/survive", "type":"bind", "source":dir.join("assets"), "options":["bind","ro","nosuid","nodev","rprivate"]},
            {"destination":"/etc/resolv.conf", "type":"bind", "source":dir.join("resolv.conf"), "options":["bind","ro","nosuid","nodev","noexec","rprivate"]}
        ],
        "linux": {
            "rootfsPropagation":"private",
            "namespaces": [{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"},{"type":"cgroup"},{"type":"user"},{"type":"network"}],
            "uidMappings":[{"containerID":0,"hostID":uid,"size":65536}],
            "gidMappings":[{"containerID":0,"hostID":uid,"size":65536}],
            "cgroupsPath":format!("/survive/{}", spec.runtime_id()),
            "resources": {"memory":{"limit":spec.limits.memory_bytes,"swap":spec.limits.memory_bytes}, "cpu":{"period":100000,"quota":spec.limits.cpu_millis*100}, "pids":{"limit":spec.limits.pids}, "unified":{"memory.oom.group":"1","memory.swap.max":"0"}, "devices":[{"allow":false,"access":"rwm"},{"allow":true,"type":"c","major":1,"minor":3,"access":"rw"},{"allow":true,"type":"c","major":1,"minor":5,"access":"rw"},{"allow":true,"type":"c","major":1,"minor":7,"access":"rw"},{"allow":true,"type":"c","major":1,"minor":8,"access":"rw"},{"allow":true,"type":"c","major":1,"minor":9,"access":"rw"},{"allow":true,"type":"c","major":5,"minor":0,"access":"rw"},{"allow":true,"type":"c","major":5,"minor":2,"access":"rw"},{"allow":true,"type":"c","major":136,"access":"rw"}]},
            "maskedPaths":["/proc/acpi","/proc/asound","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/timer_list","/proc/timer_stats","/proc/sched_debug","/proc/scsi","/proc/interrupts"],
            "readonlyPaths":["/proc/bus","/proc/fs","/proc/irq","/proc/sys","/proc/sysrq-trigger"],
            "seccomp": {"defaultAction":"SCMP_ACT_ERRNO", "defaultErrnoRet":1, "syscalls":[
                {"names":allow.split_whitespace().collect::<Vec<_>>(),"action":"SCMP_ACT_ALLOW"},
                {"names":["clone"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":33554432,"op":"SCMP_CMP_LT"}]},
                {"names":["clone3"],"action":"SCMP_ACT_ERRNO","errnoRet":38},
                {"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":1,"op":"SCMP_CMP_EQ"}]},
                {"names":["socket"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":2,"op":"SCMP_CMP_EQ"}]}
            ]}
        }
    })
}
