//! What's running in the foreground of a PTY, and whether it's `ssh`.
//!
//! The name comes from the terminal's foreground process group — the same
//! lookup the cwd poll uses, so it needs no shell cooperation: on macOS
//! `proc_pidinfo(PROC_PIDTBSDINFO)`, on Linux `/proc/<pid>/comm`. The ssh
//! host comes from the process's argument vector (`KERN_PROCARGS2` on
//! macOS, `/proc/<pid>/cmdline` on Linux), which the kernel only lets us
//! read for our own processes; a remote-owned or setuid one degrades to
//! name-only.

use std::os::fd::RawFd;

/// The foreground process on a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    /// The executable's name (`zsh`, `vim`, `ssh`).
    pub name: String,
    /// For `ssh`: the destination host, without any `user@`.
    pub ssh_host: Option<String>,
}

impl ForegroundProcess {
    /// Whether this is an interactive shell rather than a program the
    /// user ran — shells aren't worth naming in a tab title.
    pub fn is_shell(&self) -> bool {
        let name = self.name.trim_start_matches('-');
        matches!(
            name,
            "sh" | "bash"
                | "zsh"
                | "fish"
                | "dash"
                | "ksh"
                | "mksh"
                | "tcsh"
                | "csh"
                | "nu"
                | "elvish"
                | "xonsh"
                | "login"
        ) || name.starts_with("bash-")
            || name.starts_with("zsh-")
    }

    /// A short label: `ssh prod-web` for ssh, else the bare name.
    pub fn label(&self) -> String {
        match &self.ssh_host {
            Some(host) => format!("ssh {host}"),
            None => self.name.clone(),
        }
    }
}

/// The foreground process group's leader on `master_fd`.
pub fn foreground(master_fd: RawFd) -> Option<ForegroundProcess> {
    let pid = unsafe { libc::tcgetpgrp(master_fd) };
    if pid <= 0 {
        return None;
    }
    let name = process_name(pid)?;
    let ssh_host = if name == "ssh" {
        process_args(pid).and_then(|args| ssh_host_from_args(&args))
    } else {
        None
    };
    Some(ForegroundProcess { name, ssh_host })
}

/// The executable's name from procfs. `comm` is what the kernel calls the
/// process (the same 15-char-truncated field `ps` shows); the `exe` link
/// has the untruncated basename, so prefer that when it's readable and
/// agrees with `comm`'s prefix — a script's `exe` says `python3`, and
/// `comm` says the script, so a disagreement keeps `comm`.
#[cfg(target_os = "linux")]
fn process_name(pid: i32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    if comm.is_empty() {
        return None;
    }
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|n| n.trim_end_matches(" (deleted)").to_string());
    Some(match exe {
        // A login shell renames itself `-zsh`; comm keeps the dash.
        Some(full) if full.len() > comm.len() && full.starts_with(comm.trim_start_matches('-')) => {
            if comm.starts_with('-') {
                format!("-{full}")
            } else {
                full
            }
        }
        _ => comm.to_string(),
    })
}

/// The process's argv, from `/proc/<pid>/cmdline`. None when the kernel
/// won't tell us (another user's process) or the process is a zombie.
#[cfg(target_os = "linux")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(parse_cmdline(&raw))
}

/// `/proc/<pid>/cmdline`: NUL-terminated arguments, back to back. A
/// process that rewrote its argv may leave the trailing NUL off.
#[cfg(any(target_os = "linux", test))]
fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "macos")]
fn process_name(pid: i32) -> Option<String> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if written <= 0 {
            return None;
        }
        // pbi_name holds the full name; pbi_comm is truncated to 16 chars.
        let full = std::ffi::CStr::from_ptr(info.pbi_name.as_ptr().cast());
        let name = full
            .to_str()
            .ok()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::ffi::CStr::from_ptr(info.pbi_comm.as_ptr().cast())
                    .to_str()
                    .ok()
                    .map(str::to_string)
            })?;
        (!name.is_empty()).then_some(name)
    }
}

/// The process's argv, via `sysctl(KERN_PROCARGS2)`. None when the kernel
/// won't tell us (another user's process).
#[cfg(target_os = "macos")]
fn process_args(pid: i32) -> Option<Vec<String>> {
    unsafe {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
            || size == 0
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        buf.truncate(size);
        parse_procargs2(&buf)
    }
}

/// `KERN_PROCARGS2` layout: `argc` as a native int, the executable path,
/// NUL padding, then `argc` NUL-terminated arguments (the environment
/// follows, which we ignore).
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let mut pos = 4;
    // Skip the exec path.
    while pos < buf.len() && buf[pos] != 0 {
        pos += 1;
    }
    // ...and the padding after it.
    while pos < buf.len() && buf[pos] == 0 {
        pos += 1;
    }
    let mut args = Vec::with_capacity(argc);
    while args.len() < argc && pos < buf.len() {
        let start = pos;
        while pos < buf.len() && buf[pos] != 0 {
            pos += 1;
        }
        args.push(String::from_utf8_lossy(&buf[start..pos]).to_string());
        pos += 1;
    }
    Some(args)
}

/// The destination host from an `ssh` argument vector: the first
/// positional argument, skipping options and the options that take a
/// value, with any `user@` and `ssh://` scheme removed.
pub fn ssh_host_from_args(args: &[String]) -> Option<String> {
    // Options whose value is the next argument (or glued on, `-p22`).
    const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpRSWw";
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return iter.next().map(|a| clean_host(a));
        }
        if let Some(flags) = arg.strip_prefix('-')
            && !flags.is_empty()
        {
            // A cluster like `-4vp 22`: the first value-taking flag eats the
            // rest of the cluster, or the next argument if nothing follows.
            let mut chars = flags.chars();
            while let Some(c) = chars.next() {
                if WITH_VALUE.contains(c) {
                    if chars.as_str().is_empty() {
                        iter.next();
                    }
                    break;
                }
            }
            continue;
        }
        return Some(clean_host(arg));
    }
    None
}

fn clean_host(arg: &str) -> String {
    let mut host = arg.strip_prefix("ssh://").unwrap_or(arg);
    if let Some((_, rest)) = host.rsplit_once('@') {
        host = rest;
    }
    // `ssh://host:22/` form.
    if let Some((h, port)) = host.split_once(':')
        && port
            .trim_end_matches('/')
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        host = h;
    }
    host.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn ssh_host_is_the_first_positional() {
        assert_eq!(
            ssh_host_from_args(&args("ssh prod-web-01")).as_deref(),
            Some("prod-web-01")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh deploy@prod-web-01")).as_deref(),
            Some("prod-web-01")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh -p 2222 -i ~/.ssh/id host uptime")).as_deref(),
            Some("host")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh -p2222 host")).as_deref(),
            Some("host")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh -4vp 22 host")).as_deref(),
            Some("host")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh -A -t -o StrictHostKeyChecking=no bastion")).as_deref(),
            Some("bastion")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh -J jump@bastion target")).as_deref(),
            Some("target")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh ssh://me@box.example.com:22/")).as_deref(),
            Some("box.example.com")
        );
        assert_eq!(
            ssh_host_from_args(&args("ssh -- -weird")).as_deref(),
            Some("-weird")
        );
        assert_eq!(ssh_host_from_args(&args("ssh -V")), None);
        assert_eq!(ssh_host_from_args(&args("ssh")), None);
    }

    #[test]
    fn procargs2_layout_parses() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3i32.to_ne_bytes());
        buf.extend_from_slice(b"/usr/bin/ssh\0\0\0");
        buf.extend_from_slice(b"ssh\0-p\0host\0HOME=/x\0");
        assert_eq!(parse_procargs2(&buf).unwrap(), args("ssh -p host"));
        assert!(parse_procargs2(b"\0\0").is_none());
    }

    #[test]
    fn cmdline_splits_on_nul() {
        assert_eq!(parse_cmdline(b"ssh\0-p\0host\0"), args("ssh -p host"));
        assert_eq!(
            parse_cmdline(b"ssh\0host"),
            args("ssh host"),
            "missing trailing NUL"
        );
        assert!(parse_cmdline(b"").is_empty());
    }

    /// The procfs path against our own process: name and argv agree with
    /// what std reports.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_reads_our_own_process() {
        let pid = std::process::id() as i32;
        let name = process_name(pid).expect("own process name");
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            exe_name.starts_with(&name) || name == exe_name,
            "name {name:?} vs exe {exe_name:?}"
        );
        let args = process_args(pid).expect("own argv");
        let expected: Vec<String> = std::env::args().collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn shells_are_recognised() {
        let shell = ForegroundProcess {
            name: "-zsh".into(),
            ssh_host: None,
        };
        assert!(shell.is_shell());
        assert!(
            ForegroundProcess {
                name: "bash".into(),
                ssh_host: None
            }
            .is_shell()
        );
        let vim = ForegroundProcess {
            name: "vim".into(),
            ssh_host: None,
        };
        assert!(!vim.is_shell());
        assert_eq!(vim.label(), "vim");
        let ssh = ForegroundProcess {
            name: "ssh".into(),
            ssh_host: Some("prod".into()),
        };
        assert_eq!(ssh.label(), "ssh prod");
    }

    /// The real lookup against our own process group: run from a test
    /// binary, the foreground process on a fresh PTY is the shell.
    #[test]
    fn foreground_of_a_live_shell_is_the_shell() {
        use crate::terminal::session::{SessionOptions, TermSize, TerminalSession};
        let size = TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: "/bin/sh".into(),
            args: vec![],
            working_directory: None,
            scrollback: 100,
            env: std::collections::HashMap::from([(
                "HISTFILE".to_string(),
                "/dev/null".to_string(),
            )]),
            images: true,
        };
        let (session, _rx) = TerminalSession::spawn(options, size).expect("spawn sh");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(fg) = session.foreground_process() {
                assert!(fg.is_shell(), "{fg:?}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no foreground process reported"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
