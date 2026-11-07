use super::events::{Event, Interest, TERMINATION, Token, monotonic};
use super::process::{RunPlan, cstring};
use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Mutex;

const ENTRIES: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
const SELF: u32 = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

fn inotify_mask(interest: Interest) -> u32 {
    match interest {
        Interest::Written => libc::IN_MODIFY | ENTRIES,
        Interest::Touched => libc::IN_MODIFY | libc::IN_ATTRIB | ENTRIES,
        Interest::Entries => ENTRIES,
        Interest::Source => libc::IN_MODIFY | libc::IN_ATTRIB | ENTRIES | SELF,
        Interest::Ancestor => ENTRIES | SELF,
    }
}

// The epoll key's top byte says what is ready; the rest is the pid or the descriptor.
const PROCESS: u64 = 1 << 56;
const READABLE: u64 = 2 << 56;
const INOTIFY: u64 = 3 << 56;
const SIGNALS: u64 = 4 << 56;
const KIND: u64 = 0xff << 56;

static BLOCKED_BEFORE: Mutex<Option<libc::sigset_t>> = Mutex::new(None);

fn block(signal: i32) {
    let mut before = BLOCKED_BEFORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        let mut only: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut only);
        libc::sigaddset(&mut only, signal);
        libc::pthread_sigmask(libc::SIG_BLOCK, &only, &mut previous);
        if before.is_none() {
            *before = Some(previous);
        }
    }
}

/// Unblocks what the queues blocked, first taking any of those signals still pending, so none
/// is delivered after the queue that wanted it is gone (as an ignored one is dropped on macOS).
pub fn restore_platform() {
    let mut before = BLOCKED_BEFORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(previous) = before.take() else { return };
    unsafe {
        let mut pending: libc::sigset_t = std::mem::zeroed();
        libc::sigpending(&mut pending);
        for signal in TERMINATION.into_iter().chain([libc::SIGTSTP, libc::SIGCHLD]) {
            if libc::sigismember(&pending, signal) == 1 && libc::sigismember(&previous, signal) == 0 {
                let mut only: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut only);
                libc::sigaddset(&mut only, signal);
                let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                libc::sigtimedwait(&only, std::ptr::null_mut(), &zero);
            }
        }
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
    }
}

struct Inotify {
    fd: OwnedFd,
    /// Each kernel watch descriptor with the tokens of the watches sharing it: the kernel gives one
    /// descriptor per inode, however often it is watched.
    tokens: RefCell<HashMap<i32, Vec<u64>>>,
    next: RefCell<u64>,
}

pub struct Watch {
    inotify: Rc<Inotify>,
    descriptor: i32,
    token: u64,
}

impl Watch {
    pub fn token(&self) -> Token {
        Token(self.token)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut tokens = self.inotify.tokens.borrow_mut();
        if let Some(sharing) = tokens.get_mut(&self.descriptor) {
            sharing.retain(|token| *token != self.token);
            if sharing.is_empty() {
                tokens.remove(&self.descriptor);
                unsafe { libc::inotify_rm_watch(self.inotify.fd.as_raw_fd(), self.descriptor) };
            }
        }
    }
}

pub struct Queue {
    epoll: OwnedFd,
    inotify: RefCell<Option<Rc<Inotify>>>,
    signals: RefCell<Option<(OwnedFd, libc::sigset_t)>>,
    processes: RefCell<HashMap<i32, OwnedFd>>,
}

fn owned(fd: i32, what: &str) -> Result<OwnedFd> {
    if fd < 0 {
        return Err(Error::os(what));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Queue {
    pub fn new() -> Result<Self> {
        let epoll = owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) }, "epoll")?;
        Ok(Queue {
            epoll,
            inotify: RefCell::new(None),
            signals: RefCell::new(None),
            processes: RefCell::new(HashMap::new()),
        })
    }

    /// Edge-triggered, as kqueue's EV_CLEAR: a descriptor reports again only on new readiness.
    fn add(&self, fd: RawFd, key: u64) -> io::Result<()> {
        let mut event = libc::epoll_event { events: (libc::EPOLLIN | libc::EPOLLET) as u32, u64: key };
        if unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn process(&self, pid: i32) -> Result<bool> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(false),
                _ => Err(Error::io("register process", error)),
            };
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        self.add(fd.as_raw_fd(), PROCESS | pid as u64).map_err(|error| Error::io("register process", error))?;
        self.processes.borrow_mut().insert(pid, fd);
        Ok(true)
    }

    fn inotify(&self) -> io::Result<Rc<Inotify>> {
        if let Some(inotify) = self.inotify.borrow().as_ref() {
            return Ok(inotify.clone());
        }
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        self.add(fd.as_raw_fd(), INOTIFY)?;
        let inotify = Rc::new(Inotify { fd, tokens: RefCell::new(HashMap::new()), next: RefCell::new(0) });
        *self.inotify.borrow_mut() = Some(inotify.clone());
        Ok(inotify)
    }

    pub fn watch(&self, path: &Path, follow: bool, interest: Interest) -> io::Result<Watch> {
        let inotify = self.inotify()?;
        let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let mut mask = inotify_mask(interest) | libc::IN_MASK_ADD;
        if !follow {
            mask |= libc::IN_DONT_FOLLOW;
        }
        let descriptor = unsafe { libc::inotify_add_watch(inotify.fd.as_raw_fd(), name.as_ptr(), mask) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let token = {
            let mut next = inotify.next.borrow_mut();
            *next += 1;
            *next
        };
        inotify.tokens.borrow_mut().entry(descriptor).or_default().push(token);
        Ok(Watch { inotify, descriptor, token })
    }

    pub fn watch_file(&self, file: &fs::File, interest: Interest) -> io::Result<Watch> {
        self.watch(Path::new(&format!("/proc/self/fd/{}", file.as_raw_fd())), true, interest)
    }

    pub fn readable(&self, fd: RawFd) -> Result<()> {
        self.add(fd, READABLE | fd as u64).map_err(|error| Error::io("register pipe", error))
    }

    fn signal(&self, signal: i32) -> Result<()> {
        block(signal);
        let mut signals = self.signals.borrow_mut();
        match signals.as_mut() {
            Some((fd, mask)) => unsafe {
                libc::sigaddset(mask, signal);
                if libc::signalfd(fd.as_raw_fd(), mask, 0) < 0 {
                    return Err(Error::os("register signal"));
                }
            },
            None => {
                let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, signal);
                }
                let fd = owned(
                    unsafe { libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) },
                    "register signal",
                )?;
                self.add(fd.as_raw_fd(), SIGNALS).map_err(|error| Error::io("register signal", error))?;
                *signals = Some((fd, mask));
            }
        }
        Ok(())
    }

    pub fn termination(&self) -> Result<()> {
        for signal in TERMINATION {
            self.signal(signal)?;
        }
        Ok(())
    }

    /// Must come before the child is started: a stop before it would go unreported.
    pub fn children(&self) -> Result<()> {
        self.signal(libc::SIGCHLD)
    }

    pub fn suspension(&self) -> Result<()> {
        self.signal(libc::SIGTSTP)
    }

    pub fn wait(&self, seconds: Option<f64>) -> Result<Vec<Event>> {
        let deadline = seconds.map(|seconds| monotonic() + seconds);
        loop {
            let timeout = deadline.map_or(-1, |deadline| ((deadline - monotonic()).max(0.0) * 1000.0).ceil() as i32);
            let mut ready: Vec<libc::epoll_event> = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
            let count = unsafe { libc::epoll_wait(self.epoll.as_raw_fd(), ready.as_mut_ptr(), 64, timeout) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(Error::io("wait for event", error));
            }
            let mut found = Vec::new();
            for event in &ready[..count as usize] {
                let key = event.u64;
                match key & KIND {
                    PROCESS => found.push(Event::Exited((key & !KIND) as i32)),
                    READABLE => found.push(Event::Readable((key & !KIND) as RawFd)),
                    INOTIFY => self.read_inotify(&mut found)?,
                    SIGNALS => self.read_signals(&mut found)?,
                    _ => {}
                }
            }
            return Ok(found);
        }
    }

    fn read_inotify(&self, found: &mut Vec<Event>) -> Result<()> {
        let Some(inotify) = self.inotify.borrow().clone() else { return Ok(()) };
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let count =
                unsafe { libc::read(inotify.fd.as_raw_fd(), buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::EAGAIN) => Ok(()),
                    Some(libc::EINTR) => continue,
                    _ => Err(Error::io("read file events", error)),
                };
            }
            let mut at = 0;
            let header = std::mem::size_of::<libc::inotify_event>();
            while at + header <= count as usize {
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buffer[at..].as_ptr() as *const libc::inotify_event) };
                at += header + event.len as usize;
                let tokens = inotify.tokens.borrow();
                // A lost event could be any watch's, so every watch hears of it: re-reading a source is
                // never wrong, missing its change would be.
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    found.extend(tokens.values().flatten().map(|token| Event::Changed(Token(*token))));
                } else if let Some(sharing) = tokens.get(&event.wd) {
                    found.extend(sharing.iter().map(|token| Event::Changed(Token(*token))));
                }
            }
        }
    }

    fn read_signals(&self, found: &mut Vec<Event>) -> Result<()> {
        let signals = self.signals.borrow();
        let Some((fd, _)) = signals.as_ref() else { return Ok(()) };
        loop {
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::signalfd_siginfo>();
            let count = unsafe { libc::read(fd.as_raw_fd(), &mut info as *mut _ as *mut libc::c_void, size) };
            if count < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::EAGAIN) => Ok(()),
                    Some(libc::EINTR) => continue,
                    _ => Err(Error::io("read signals", error)),
                };
            }
            match info.ssi_signo as i32 {
                libc::SIGCHLD => found.push(Event::Child),
                libc::SIGTSTP => found.push(Event::Suspend),
                signal => return Err(Error::Interrupted(signal)),
            }
        }
    }
}

/// The fields of a siginfo_t that waitid fills for a child, as glibc lays them out.
#[repr(C)]
struct ChildInfo {
    signo: i32,
    errno: i32,
    code: i32,
    #[cfg(target_pointer_width = "64")]
    pad: i32,
    pid: i32,
    uid: u32,
    status: i32,
    rest: [u8; 100],
}

fn wait_child(pid: i32, flags: i32) -> Option<ChildInfo> {
    let mut info: ChildInfo = unsafe { std::mem::zeroed() };
    loop {
        let result =
            unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info as *mut _ as *mut libc::siginfo_t, flags) };
        if result == 0 {
            return Some(info);
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
}

pub fn stopped(pid: i32) -> Option<i32> {
    let info = wait_child(pid, libc::WSTOPPED | libc::WNOHANG)?;
    (info.pid == pid && info.code == libc::CLD_STOPPED).then_some(info.status)
}

pub fn stopped_or_exited(pid: i32) -> bool {
    wait_child(pid, libc::WSTOPPED | libc::WEXITED | libc::WNOWAIT).is_some_and(|info| info.code == libc::CLD_STOPPED)
}

/// The fields of /proc/PID/stat after the command name, which may itself hold spaces.
fn stat_fields(pid: i32) -> io::Result<Vec<String>> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after = text.rfind(')').map_or("", |at| &text[at + 1..]);
    Ok(after.split_whitespace().map(String::from).collect())
}

pub fn alive(pid: i32) -> Result<bool> {
    match stat_fields(pid) {
        Ok(fields) => Ok(fields.first().is_some_and(|state| state != "Z" && state != "X")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(false),
        Err(error) => Err(Error::io("describe process", error)),
    }
}

pub fn group_members(group: i32) -> Result<Vec<i32>> {
    let entries = fs::read_dir("/proc").map_err(|error| Error::io("list process group", error))?;
    let mut members = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        if let Ok(fields) = stat_fields(pid)
            && fields.get(2).and_then(|pgrp| pgrp.parse::<i32>().ok()) == Some(group)
        {
            members.push(pid);
        }
    }
    Ok(members)
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
        let mut files: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        let mut attributes: libc::posix_spawnattr_t = std::mem::zeroed();
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
            libc::posix_spawn_file_actions_addchdir_np(&mut files, directory.as_ptr());
        }
        // Own process group; default dispositions and an empty mask, as this process blocks and ignores some.
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
