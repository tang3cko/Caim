use super::events::{Event, Interest, TERMINATION, Token, ignore, monotonic};
use super::process::{RunPlan, cstring};
use crate::error::{Error, Result};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// libproc's PROC_PGRP_ONLY, which the libc crate does not carry.
const PROC_PGRP_ONLY: u32 = 2;

fn vnode_flags(interest: Interest) -> u32 {
    let written = libc::NOTE_WRITE | libc::NOTE_EXTEND;
    match interest {
        Interest::Written => written,
        Interest::Touched => written | libc::NOTE_ATTRIB,
        Interest::Entries => libc::NOTE_WRITE,
        Interest::Source => written | libc::NOTE_ATTRIB | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
        Interest::Ancestor => libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
    }
}

fn open_event_only(path: &Path, follow: bool) -> io::Result<OwnedFd> {
    let flags = libc::O_EVTONLY | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW };
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe { libc::open(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub struct Watch {
    fd: OwnedFd,
}

impl Watch {
    pub fn token(&self) -> Token {
        Token(self.fd.as_raw_fd() as u64)
    }
}

pub struct Queue {
    fd: OwnedFd,
}

impl Queue {
    pub fn new() -> Result<Self> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(Error::os("kqueue"));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(Error::os("kqueue close-on-exec"));
        }
        Ok(Queue { fd })
    }

    fn add(&self, ident: usize, filter: i16, flags: u32) -> io::Result<()> {
        let event = libc::kevent {
            ident,
            filter,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: flags,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let result = unsafe { libc::kevent(self.fd.as_raw_fd(), &event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// False when the process has already gone: the kernel refuses to register it.
    pub fn process(&self, pid: i32) -> Result<bool> {
        match self.add(pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT) {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(error) => Err(Error::io("register process", error)),
        }
    }

    pub fn watch(&self, path: &Path, follow: bool, interest: Interest) -> io::Result<Watch> {
        let fd = open_event_only(path, follow)?;
        self.add(fd.as_raw_fd() as usize, libc::EVFILT_VNODE, vnode_flags(interest))?;
        Ok(Watch { fd })
    }

    pub fn readable(&self, fd: RawFd) -> Result<()> {
        self.add(fd as usize, libc::EVFILT_READ, 0).map_err(|error| Error::io("register pipe", error))
    }

    pub fn termination(&self) -> Result<()> {
        for signal in TERMINATION {
            ignore(signal);
            self.signal(signal)?;
        }
        Ok(())
    }

    /// Must come before the child is started: a stop before it would go unreported.
    pub fn children(&self) -> Result<()> {
        self.signal(libc::SIGCHLD)
    }

    pub fn suspension(&self) -> Result<()> {
        ignore(libc::SIGTSTP);
        self.signal(libc::SIGTSTP)
    }

    fn signal(&self, signal: i32) -> Result<()> {
        self.add(signal as usize, libc::EVFILT_SIGNAL, 0).map_err(|error| Error::io("register signal", error))
    }

    pub fn wait(&self, seconds: Option<f64>) -> Result<Vec<Event>> {
        let deadline = seconds.map(|seconds| monotonic() + seconds);
        loop {
            let timeout = deadline.map(|deadline| {
                let remaining = (deadline - monotonic()).max(0.0);
                libc::timespec {
                    tv_sec: remaining.trunc() as libc::time_t,
                    tv_nsec: (remaining.fract() * 1e9) as libc::c_long,
                }
            });
            let mut events: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; 64];
            let count = unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    timeout.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(Error::io("wait for event", error));
            }
            events.truncate(count as usize);
            let mut found = Vec::with_capacity(events.len());
            for event in &events {
                if event.flags & libc::EV_ERROR != 0 {
                    return Err(Error::errno("event failed", event.data as i32));
                }
                match event.filter {
                    libc::EVFILT_SIGNAL => match event.ident as i32 {
                        libc::SIGCHLD => found.push(Event::Child),
                        libc::SIGTSTP => found.push(Event::Suspend),
                        signal => return Err(Error::Interrupted(signal)),
                    },
                    libc::EVFILT_PROC => found.push(Event::Exited(event.ident as i32)),
                    libc::EVFILT_READ => found.push(Event::Readable(event.ident as RawFd)),
                    libc::EVFILT_VNODE => found.push(Event::Changed(Token(event.ident as u64))),
                    _ => {}
                }
            }
            return Ok(found);
        }
    }
}

/// A zombie still answers kill(pid, 0) but has left; only a process the kernel says is not a zombie is alive.
pub fn alive(pid: i32) -> Result<bool> {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size)
    };
    if written <= 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH) | Some(0) | None => Ok(false),
            // A process of another user cannot be described, and is not one of ours to judge.
            Some(libc::EPERM) => Ok(true),
            _ => Err(Error::io("describe process", error)),
        };
    }
    Ok(written == size && info.pbsi_status != libc::SZOMB)
}

pub fn stopped(pid: i32) -> Option<i32> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WSTOPPED | libc::WNOHANG) };
    (result == 0 && info.si_pid == pid && info.si_code == libc::CLD_STOPPED).then_some(info.si_status)
}

pub fn stopped_or_exited(pid: i32) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let flags = libc::WSTOPPED | libc::WEXITED | libc::WNOWAIT;
        if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } == 0 {
            return info.si_code == libc::CLD_STOPPED;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

pub fn restore_platform() {}

pub fn group_members(group: i32) -> Result<Vec<i32>> {
    let mut members: Vec<libc::pid_t> = vec![0; 1024];
    loop {
        let capacity = (members.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let bytes = unsafe {
            libc::proc_listpids(PROC_PGRP_ONLY, group as u32, members.as_mut_ptr() as *mut libc::c_void, capacity)
        };
        if bytes < 0 {
            return Err(Error::os("list process group"));
        }
        let count = bytes as usize / std::mem::size_of::<libc::pid_t>();
        if count < members.len() {
            members.truncate(count);
            return Ok(members);
        }
        members.resize(members.len() * 2, 0);
    }
}

pub fn start(plan: &RunPlan, output: Option<RawFd>, environment: &[CString]) -> Result<i32> {
    let mut envp: Vec<*mut libc::c_char> = environment.iter().map(|entry| entry.as_ptr() as *mut _).collect();
    envp.push(std::ptr::null_mut());
    let program = plan.command.first().ok_or_else(|| Error::Usage("no command".into()))?;
    let arguments = plan.command.iter().map(|argument| cstring(argument)).collect::<Result<Vec<_>>>()?;
    let mut argv: Vec<*mut libc::c_char> = arguments.iter().map(|argument| argument.as_ptr() as *mut _).collect();
    argv.push(std::ptr::null_mut());
    let output_file = cstring(&plan.output)?;
    let null = cstring("/dev/null")?;
    let program = cstring(program)?;
    let directory = plan.directory.as_deref().map(cstring).transpose()?;
    unsafe {
        let mut files: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        let mut attributes: libc::posix_spawnattr_t = std::ptr::null_mut();
        if libc::posix_spawn_file_actions_init(&mut files) != 0 {
            return Err(Error::Unavailable("cannot prepare the command's start".into()));
        }
        if libc::posix_spawnattr_init(&mut attributes) != 0 {
            libc::posix_spawn_file_actions_destroy(&mut files);
            return Err(Error::Unavailable("cannot prepare the command's start".into()));
        }
        if !plan.inherit {
            libc::posix_spawn_file_actions_addopen(&mut files, 0, null.as_ptr(), libc::O_RDONLY, 0);
            match output {
                Some(write) => {
                    libc::posix_spawn_file_actions_adddup2(&mut files, write, 1);
                }
                None => {
                    libc::posix_spawn_file_actions_addopen(
                        &mut files,
                        1,
                        output_file.as_ptr(),
                        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
                        0o644,
                    );
                }
            }
            libc::posix_spawn_file_actions_adddup2(&mut files, 1, 2);
        }
        if let Some(directory) = &directory {
            posix_spawn_file_actions_addchdir_np(&mut files, directory.as_ptr());
        }
        // Own process group so one signal reaches everything it starts; default dispositions, as this process ignores some.
        let mut every: libc::sigset_t = std::mem::zeroed();
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut every);
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setflags(
            &mut attributes,
            (libc::POSIX_SPAWN_SETPGROUP | libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK) as libc::c_short,
        );
        libc::posix_spawnattr_setpgroup(&mut attributes, 0);
        libc::posix_spawnattr_setsigdefault(&mut attributes, &every);
        libc::posix_spawnattr_setsigmask(&mut attributes, &none);
        let mut pid: libc::pid_t = 0;
        let error = libc::posix_spawnp(&mut pid, program.as_ptr(), &files, &attributes, argv.as_ptr(), envp.as_ptr());
        libc::posix_spawn_file_actions_destroy(&mut files);
        libc::posix_spawnattr_destroy(&mut attributes);
        if error != 0 {
            return Err(Error::Start(
                error,
                format!("start {}: {}", plan.command[0], io::Error::from_raw_os_error(error)),
            ));
        }
        Ok(pid)
    }
}

unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}
