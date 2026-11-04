#![allow(dead_code)]
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};

pub fn caim() -> Command {
    Command::new(env!("CARGO_BIN_EXE_caim"))
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let template = std::env::temp_dir().join("caim-test.XXXXXX");
        let mut bytes = CString::new(template.to_string_lossy().into_owned()).unwrap().into_bytes_with_nul();
        assert!(!unsafe { libc::mkdtemp(bytes.as_mut_ptr() as *mut libc::c_char) }.is_null());
        bytes.pop();
        TempDir(fs::canonicalize(String::from_utf8(bytes).unwrap()).unwrap())
    }
    pub fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
    pub fn text(&self, relative: &str) -> String {
        self.path(relative).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct Waiting {
    pub child: Child,
}

impl Waiting {
    pub fn start() -> Self {
        Self::shell("IFS= read -r stop", false)
    }
    pub fn shell(script: &str, group: bool) -> Self {
        let mut command = Command::new("bash");
        command.args(["-c", &format!("echo ready; {script}")]).stdin(Stdio::piped()).stdout(Stdio::piped());
        if group {
            command.process_group(0);
        }
        let mut child = command.spawn().unwrap();
        assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "ready\n");
        Waiting { child }
    }
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }
    pub fn pid_text(&self) -> String {
        self.pid().to_string()
    }
    pub fn stop(&mut self) {
        if let Some(mut stdin) = self.child.stdin.take() {
            stdin.write_all(b"stop\n").unwrap();
        }
    }
    pub fn reap(&mut self) -> i32 {
        status_of(self.child.wait().unwrap())
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn status_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn combined(output: Output) -> (i32, String) {
    let Output { status, stdout, stderr } = output;
    (status_of(status), String::from_utf8_lossy(&stderr).into_owned() + &String::from_utf8_lossy(&stdout))
}

pub fn run(command: &mut Command) -> (i32, String) {
    combined(command.stdin(Stdio::null()).output().unwrap())
}

pub fn spawn(command: &mut Command) -> Child {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

pub fn finish(child: Child) -> (i32, String) {
    combined(child.wait_with_output().unwrap())
}

pub fn written(path: &Path) {
    let (status, output) = run(caim().args(["written", &path.to_string_lossy(), "180"]));
    assert_eq!(status, 0, "readiness did not arrive at {}: {output}", path.display());
}

pub fn append(path: &Path, text: &[u8]) {
    fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text).unwrap();
}

pub fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// One line of the child's stdout, read a byte at a time: a buffered reader would also take the
/// lines after it and lose them when dropped, leaving the next read waiting forever.
pub fn stdout_line(stdout: &mut ChildStdout) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).unwrap() == 1 {
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(line).unwrap()
}
