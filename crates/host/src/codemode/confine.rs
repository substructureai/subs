//! Locks a sandbox process away from the host before it reads a script. After
//! `confine`, the process can read stdin, write stdout, allocate memory and
//! read the clock. It cannot open a file, open a socket or start a process.

use std::io;

/// One script's CPU, past its wall-clock deadline.
const CPU_SECS: u64 = 20;
/// `nobody`.
const UNPRIVILEGED: libc::uid_t = 65534;

/// How a confined process answers a call it may not make.
#[derive(Clone, Copy, PartialEq)]
pub enum Denial {
    Kill,
    /// Fail the call, so a probe can report it.
    Refuse,
}

/// `data` caps the process's heap, in bytes.
pub fn confine(denial: Denial, data: u64) -> io::Result<()> {
    close_inherited();
    isolate();
    unprivileged()?;
    limits(data)?;
    lock(denial)
}

/// The OS lock alone, without the limits, so a probe can test it by itself.
pub fn lock_only() -> io::Result<()> {
    close_inherited();
    lock(Denial::Refuse)
}

fn close_inherited() {
    #[cfg(target_os = "linux")]
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } == 0 {
        return;
    }
    let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let max = if max < 0 { 1024 } else { max.min(1 << 16) } as libc::c_int;
    for fd in 3..max {
        unsafe { libc::close(fd) };
    }
}

/// A network namespace of its own, where the kernel allows one.
fn isolate() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS);
    }
}

fn unprivileged() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setgid(UNPRIVILEGED) } != 0 || unsafe { libc::setuid(UNPRIVILEGED) } != 0 {
        return Err(io::Error::last_os_error());
    }
    match unsafe { libc::setuid(0) } {
        0 => Err(io::Error::other("root came back after dropping it")),
        _ => Ok(()),
    }
}

fn limits(data: u64) -> io::Result<()> {
    limit(libc::RLIMIT_CORE as i64, 0)?;
    limit(libc::RLIMIT_FSIZE as i64, 0)?;
    limit(libc::RLIMIT_NOFILE as i64, 3)?;
    limit(libc::RLIMIT_CPU as i64, CPU_SECS)?;
    #[cfg(target_os = "linux")]
    limit(libc::RLIMIT_DATA as i64, data)?;
    let _ = data;
    Ok(())
}

/// Lowers the process's CPU limit to what it has used, `ms` more, and a second
/// past that, so the OS stops a run the interpreter could not.
pub fn limit_cpu(ms: u64) -> io::Result<()> {
    let mut used = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut used) };
    let used_ms = used.tv_sec as u64 * 1000 + used.tv_nsec as u64 / 1_000_000;
    let secs = (used_ms + ms).div_ceil(1000) + 1;
    let limit = libc::rlimit {
        rlim_cur: secs as libc::rlim_t,
        rlim_max: secs as libc::rlim_t,
    };
    #[cfg(target_os = "linux")]
    let set = unsafe {
        libc::syscall(
            libc::SYS_prlimit64,
            0,
            libc::RLIMIT_CPU,
            &limit as *const libc::rlimit,
            std::ptr::null_mut::<libc::rlimit>(),
        )
    } as libc::c_int;
    #[cfg(not(target_os = "linux"))]
    let set = unsafe { libc::setrlimit(libc::RLIMIT_CPU, &limit) };
    match set {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn limit(resource: i64, value: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    match unsafe { libc::setrlimit(resource as _, &limit) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(target_os = "macos")]
fn lock(_: Denial) -> io::Result<()> {
    use std::ffi::{CStr, CString};

    extern "C" {
        fn sandbox_init(
            profile: *const libc::c_char,
            flags: u64,
            errorbuf: *mut *mut libc::c_char,
        ) -> libc::c_int;
        fn sandbox_free_error(errorbuf: *mut libc::c_char);
    }

    let profile = CString::new(
        "(version 1)\n(deny default)\n(deny process-info*)\n(deny signal)\n(deny mach*)\n\
         (deny ipc*)\n(deny sysctl*)\n",
    )
    .expect("no NUL");
    let mut error = std::ptr::null_mut();
    if unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) } == 0 {
        return Ok(());
    }
    let message = match error.is_null() {
        true => "sandbox_init failed".to_string(),
        false => {
            let message = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(error) };
            message
        }
    };
    Err(io::Error::other(message))
}

#[cfg(target_os = "linux")]
fn lock(denial: Denial) -> io::Result<()> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };

    #[cfg(target_arch = "x86_64")]
    const ARCH: TargetArch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    const ARCH: TargetArch = TargetArch::aarch64;

    let allowed = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_brk,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_mprotect,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_clock_gettime,
        libc::SYS_gettimeofday,
        libc::SYS_getrandom,
        libc::SYS_sched_yield,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_restart_syscall,
    ];
    let own = |arg| -> io::Result<Vec<SeccompRule>> {
        let pid = unsafe { libc::getpid() } as u64;
        let only = SeccompCondition::new(arg, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, pid)
            .map_err(io::Error::other)?;
        Ok(vec![SeccompRule::new(vec![only]).map_err(io::Error::other)?])
    };
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
        allowed.iter().map(|&call| (call, Vec::new())).collect();
    rules.insert(libc::SYS_tkill, own(0)?);
    rules.insert(libc::SYS_tgkill, own(0)?);
    let itself = SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, 0)
        .map_err(io::Error::other)?;
    rules.insert(
        libc::SYS_prlimit64,
        vec![SeccompRule::new(vec![itself]).map_err(io::Error::other)?],
    );
    let mismatch = match denial {
        Denial::Kill => SeccompAction::KillProcess,
        Denial::Refuse => SeccompAction::Errno(libc::EPERM as u32),
    };
    let filter = SeccompFilter::new(rules, mismatch, SeccompAction::Allow, ARCH)
        .map_err(io::Error::other)?;
    let program: BpfProgram = filter.try_into().map_err(io::Error::other)?;
    seccompiler::apply_filter(&program).map_err(io::Error::other)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn lock(_: Denial) -> io::Result<()> {
    Err(io::Error::other(
        "this platform cannot confine a sandbox process",
    ))
}

/// What a confined process can still reach. Each entry should be denied.
pub fn probe() -> serde_json::Value {
    fn denied<T>(result: io::Result<T>) -> String {
        match result {
            Ok(_) => "allowed".to_string(),
            Err(e) => format!("denied: {e}"),
        }
    }
    serde_json::json!({
        "read_file": denied(std::fs::read("/etc/hosts")),
        "proc_environ": denied(std::fs::read("/proc/self/environ")),
        "parent_environ": denied(parent_environ()),
        "write_file": denied(std::fs::write(std::env::temp_dir().join("subs-sandbox-probe"), b"x")),
        "list_dir": denied(std::fs::read_dir("/")),
        "tcp": denied(std::net::TcpStream::connect("1.1.1.1:53")),
        "udp": denied(std::net::UdpSocket::bind("0.0.0.0:0")),
        "unix_socket": denied(std::os::unix::net::UnixStream::connect("/var/run/syslog")),
        "signal_parent": denied(match unsafe { libc::kill(libc::getppid(), 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }),
        "spawn": denied(std::process::Command::new("/bin/sh").arg("-c").arg("true").status()),
        "env": std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "uid": unsafe { libc::geteuid() },
    })
}

#[cfg(target_os = "linux")]
fn parent_environ() -> io::Result<Vec<u8>> {
    std::fs::read(format!("/proc/{}/environ", unsafe { libc::getppid() }))
}

/// The parent's arguments and environment, as `ps eww` reads them.
#[cfg(target_os = "macos")]
fn parent_environ() -> io::Result<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, unsafe {
        libc::getppid()
    }];
    let mut buf = vec![0u8; 1 << 16];
    let mut len = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    match rc {
        0 => Ok(buf[..len].to_vec()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_environ() -> io::Result<Vec<u8>> {
    Err(io::Error::other("no probe on this platform"))
}
