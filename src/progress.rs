use crate::error::{Error, Result};
use std::collections::{BTreeSet, HashSet};

pub const STALL_SECONDS: f64 = 180.0;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// So that a counter, a timestamp or a padded column does not make a repeated line look new.
pub fn line_shape(line: &[u8]) -> Vec<u8> {
    let mut shape = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if is_space(line[i]) {
            i += 1;
            continue;
        }
        if !line[i].is_ascii_digit() {
            shape.push(line[i]);
            i += 1;
            continue;
        }
        shape.push(b'#');
        while i < line.len() && line[i].is_ascii_digit() {
            i += 1;
        }
    }
    shape
}

const LONGEST_LINE: usize = 64 * 1024;
const SHAPES_KEPT: usize = 65_536;

#[derive(Default)]
pub struct Written {
    pending: Vec<u8>,
    shapes: HashSet<Vec<u8>>,
    binary: bool,
    stream: bool,
}

impl Written {
    pub fn stream() -> Self {
        Written { stream: true, ..Written::default() }
    }

    pub fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() || self.binary {
            return false;
        }
        // A binary output is progress when it appears; a growing one (a profiler capture) says no more than a repeating line.
        if !self.stream && bytes.contains(&0) {
            self.binary = true;
            return true;
        }
        self.pending.extend_from_slice(bytes);
        let mut progress = false;
        let mut begin = 0;
        // A progress display rewrites its line after a carriage return; each rewrite is a line.
        while let Some(offset) = self.pending[begin..].iter().position(|&c| c == b'\n' || c == b'\r') {
            let end = begin + offset;
            progress |= self.judge(begin, end);
            begin = end + 1;
        }
        if self.pending.len() - begin > LONGEST_LINE {
            let end = self.pending.len();
            progress |= self.judge(begin, end);
            begin = end;
        }
        self.pending.drain(..begin);
        progress
    }

    fn judge(&mut self, begin: usize, end: usize) -> bool {
        let line = &self.pending[begin..end];
        let shape = if line.contains(&0) { Vec::new() } else { line_shape(line) };
        if shape.is_empty() || self.shapes.contains(&shape) {
            return false;
        }
        // Starting over keeps memory bounded; a loop of a few repeated lines is caught again at once.
        if self.shapes.len() >= SHAPES_KEPT {
            self.shapes.clear();
        }
        self.shapes.insert(shape)
    }

    pub fn restart(&mut self) {
        self.pending.clear();
        self.binary = false;
    }
}

pub struct StallClock {
    last_progress: f64,
    interval: f64,
}

impl StallClock {
    pub fn new(started: f64, idle: f64) -> Result<Self> {
        if !started.is_finite() || !idle.is_finite() || idle <= 0.0 {
            return Err(Error::Usage("stall interval must be finite and positive".into()));
        }
        Ok(StallClock { last_progress: started, interval: idle })
    }

    pub fn observe(&mut self, now: f64, progress: bool, deferred: bool) -> bool {
        if progress || (self.remaining(now) == 0.0 && deferred) {
            self.last_progress = now;
        }
        self.remaining(now) == 0.0
    }

    pub fn pause(&mut self, seconds: f64) {
        self.last_progress += seconds.max(0.0);
    }

    pub fn remaining(&self, now: f64) -> f64 {
        (self.last_progress + self.interval - now).max(0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileState {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub kind: Kind,
}

impl FileState {
    pub fn same_file(&self, other: &FileState) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

pub trait FileSystem {
    fn stat(&mut self, path: &str) -> Result<Option<FileState>>;
    fn list(&mut self, path: &str) -> Result<Option<Vec<String>>>;
    fn read(&mut self, path: &str, offset: u64, count: u64) -> Result<Option<Vec<u8>>>;
}

pub struct Appended {
    file: String,
    identity: Option<FileState>,
    offset: u64,
}

#[derive(Default, Debug)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub restarted: bool,
    pub more: bool,
}

impl Appended {
    pub fn new(path: impl Into<String>) -> Self {
        Appended { file: path.into(), identity: None, offset: 0 }
    }

    pub fn path(&self) -> &str {
        &self.file
    }

    pub fn baseline(&mut self, filesystem: &mut dyn FileSystem) -> Result<()> {
        self.identity = filesystem.stat(&self.file)?.filter(|state| state.kind == Kind::File);
        self.offset = self.identity.map_or(0, |state| state.size);
        Ok(())
    }

    pub fn next(&mut self, filesystem: &mut dyn FileSystem, budget: u64) -> Result<Chunk> {
        let mut chunk = Chunk::default();
        let state = match filesystem.stat(&self.file)? {
            Some(state) if state.kind == Kind::File => state,
            _ => {
                self.identity = None;
                self.offset = 0;
                return Ok(chunk);
            }
        };
        if !self.identity.is_some_and(|known| known.same_file(&state)) || state.size < self.offset {
            chunk.restarted = true;
            self.offset = 0;
        }
        self.identity = Some(state);
        if self.offset >= state.size || budget == 0 {
            return Ok(chunk);
        }
        let Some(bytes) = filesystem.read(&self.file, self.offset, budget.min(state.size - self.offset))? else {
            return Ok(chunk);
        };
        self.offset += bytes.len() as u64;
        chunk.more = self.offset < state.size;
        chunk.bytes = bytes;
        Ok(chunk)
    }
}

pub struct LogFollower {
    input: Appended,
    written: Written,
}

impl LogFollower {
    pub fn new(path: impl Into<String>) -> Self {
        LogFollower { input: Appended::new(path), written: Written::default() }
    }

    pub fn path(&self) -> &str {
        self.input.path()
    }

    pub fn baseline(&mut self, filesystem: &mut dyn FileSystem) -> Result<()> {
        self.input.baseline(filesystem)
    }

    pub fn advance(&mut self, filesystem: &mut dyn FileSystem, budget: u64) -> Result<(bool, bool)> {
        let chunk = self.input.next(filesystem, budget)?;
        if chunk.restarted {
            self.written.restart();
        }
        Ok((self.written.append(&chunk.bytes), chunk.more))
    }
}

pub struct DirectoryEntries {
    directory: String,
    names: BTreeSet<String>,
}

impl DirectoryEntries {
    pub fn new(path: impl Into<String>) -> Self {
        DirectoryEntries { directory: path.into(), names: BTreeSet::new() }
    }

    pub fn path(&self) -> &str {
        &self.directory
    }

    pub fn baseline(&mut self, filesystem: &mut dyn FileSystem) -> Result<()> {
        self.names = filesystem.list(&self.directory)?.unwrap_or_default().into_iter().collect();
        Ok(())
    }

    pub fn refresh(&mut self, filesystem: &mut dyn FileSystem) -> Result<bool> {
        let Some(current) = filesystem.list(&self.directory)? else {
            self.names.clear();
            return Ok(false);
        };
        let now: BTreeSet<String> = current.into_iter().collect();
        let progress = now.iter().any(|name| !self.names.contains(name));
        self.names = now;
        Ok(progress)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeclarationKind {
    Log,
    Dir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub kind: DeclarationKind,
    pub path: String,
}

#[derive(Default)]
pub struct Declarations {
    pending: Vec<u8>,
}

impl Declarations {
    pub fn append(&mut self, bytes: &[u8]) -> Result<Vec<Declaration>> {
        self.pending.extend_from_slice(bytes);
        let mut found = Vec::new();
        let mut begin = 0;
        while let Some(offset) = self.pending[begin..].iter().position(|&c| c == b'\n') {
            let end = begin + offset;
            let mut line = &self.pending[begin..end];
            begin = end + 1;
            while let [rest @ .., b'\r'] = line {
                line = rest;
            }
            if line.iter().all(|&c| c == b' ' || c == b'\t') {
                continue;
            }
            let text = String::from_utf8_lossy(line).into_owned();
            let kind = if text.starts_with("log ") {
                DeclarationKind::Log
            } else if text.starts_with("dir ") {
                DeclarationKind::Dir
            } else {
                return Err(Error::Unavailable(format!("unknown watch declaration: {text}")));
            };
            let path = text[4..].to_string();
            if !path.starts_with('/') {
                return Err(Error::Unavailable(format!("watch declaration needs an absolute path: {text}")));
            }
            found.push(Declaration { kind, path });
        }
        self.pending.drain(..begin);
        Ok(found)
    }

    pub fn restart(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    type Hook = Box<dyn FnMut(&mut BTreeMap<String, Entry>, &str) -> Result<()>>;

    #[derive(Clone)]
    struct Entry {
        state: FileState,
        bytes: Vec<u8>,
    }

    struct Files {
        entries: BTreeMap<String, Entry>,
        before_list: Option<Hook>,
        before_read: Option<Hook>,
        next_inode: u64,
        bytes_read: u64,
    }

    impl Files {
        fn new() -> Self {
            let mut files =
                Files { entries: BTreeMap::new(), before_list: None, before_read: None, next_inode: 1, bytes_read: 0 };
            files.directory("/tree");
            files
        }
        fn directory(&mut self, path: &str) {
            let inode = self.next_inode;
            self.next_inode += 1;
            self.entries.insert(
                path.into(),
                Entry { state: FileState { device: 1, inode, size: 0, kind: Kind::Directory }, bytes: vec![] },
            );
        }
        fn write(&mut self, path: &str, bytes: &str) {
            let inode = match self.entries.get(path) {
                Some(entry) => entry.state.inode,
                None => {
                    self.next_inode += 1;
                    self.next_inode - 1
                }
            };
            self.entries.insert(
                path.into(),
                Entry {
                    state: FileState { device: 1, inode, size: bytes.len() as u64, kind: Kind::File },
                    bytes: bytes.as_bytes().to_vec(),
                },
            );
        }
        fn append(&mut self, path: &str, bytes: &str) {
            let old =
                self.entries.get(path).map(|entry| String::from_utf8(entry.bytes.clone()).unwrap()).unwrap_or_default();
            self.write(path, &(old + bytes));
        }
        fn replace(&mut self, path: &str, bytes: &str) {
            self.entries.remove(path);
            self.write(path, bytes);
        }
    }

    impl FileSystem for Files {
        fn stat(&mut self, path: &str) -> Result<Option<FileState>> {
            Ok(self.entries.get(path).map(|entry| entry.state))
        }
        fn list(&mut self, path: &str) -> Result<Option<Vec<String>>> {
            if let Some(mut hook) = self.before_list.take() {
                let result = hook(&mut self.entries, path);
                self.before_list = Some(hook);
                result?;
            }
            if !self.entries.contains_key(path) {
                return Ok(None);
            }
            let prefix = format!("{path}/");
            Ok(Some(
                self.entries
                    .keys()
                    .filter_map(|name| name.strip_prefix(&prefix))
                    .filter(|relative| !relative.contains('/'))
                    .map(String::from)
                    .collect(),
            ))
        }
        fn read(&mut self, path: &str, offset: u64, count: u64) -> Result<Option<Vec<u8>>> {
            if let Some(mut hook) = self.before_read.take() {
                let result = hook(&mut self.entries, path);
                self.before_read = Some(hook);
                result?;
            }
            let Some(entry) = self.entries.get(path) else { return Ok(None) };
            let start = (offset as usize).min(entry.bytes.len());
            let end = (start + count as usize).min(entry.bytes.len());
            let bytes = entry.bytes[start..end].to_vec();
            self.bytes_read += bytes.len() as u64;
            Ok(Some(bytes))
        }
    }

    fn drain(log: &mut LogFollower, fs: &mut Files) -> bool {
        let mut progress = false;
        loop {
            let (step, more) = log.advance(fs, 1 << 20).unwrap();
            progress |= step;
            if !more {
                return progress;
            }
        }
    }

    fn shape(line: &str) -> String {
        String::from_utf8(line_shape(line.as_bytes())).unwrap()
    }

    #[test]
    fn line_shapes_ignore_numbers_and_spacing() {
        let first = shape("[2026.10.21-15.20.00:014][ 78]status frames=23700 count=1");
        assert_eq!(first, "[#.#.#-#.#.#:#][#]statusframes=#count=#");
        assert_eq!(first, shape("[2026.10.21-15.31.13:053][201]status frames=63000 count=1\r"));
        assert_ne!(first, shape("waiting for the arena"));
        assert_eq!(shape("  12:00:01 compiling   a.c"), shape("12:00:02 compiling a.c"));
        assert_eq!(shape("file12 item-3.5\r\r"), "file#item-#.#");
        assert_eq!(shape("[Category][17]value=2"), "[Category][#]value=#");
        assert_eq!(shape(" \t "), "");
        assert_eq!(shape(""), "");
    }

    #[test]
    fn only_new_complete_nonblank_line_shapes_count() {
        let mut text = Written::default();
        assert!(!text.append(b"  \r\n\t\n"));
        assert!(!text.append(b"status frame="));
        assert!(text.append(b"100\n"));
        assert!(!text.append(b"status frame=200\n"));
        assert!(text.append(b"waiting for next step\n"));
        text.restart();
        assert!(!text.append(b"status frame=300\n"));
        assert!(text.append(b"completed\n"));
    }

    #[test]
    fn a_binary_output_counts_once_not_for_every_byte_a_stream_appends() {
        let mut bytes = Written::default();
        assert!(bytes.append(b"\0\x01\x02"));
        assert!(!bytes.append(b"\0\x01\x02"));
        assert!(!bytes.append(b"more"));
        assert!(!bytes.append(b""));
    }

    #[test]
    fn a_carriage_return_ends_a_line_like_a_newline() {
        let mut output = Written::stream();
        assert!(!output.append(b"\rdownloading part-a 10%"));
        assert!(output.append(b"\rdownloading part-a 55%"));
        assert!(!output.append(b"\rdownloading part-b 10%"));
        assert!(output.append(b"\r\n"));
    }

    #[test]
    fn a_line_with_no_end_is_judged_once_it_is_long() {
        let mut output = Written::stream();
        let long = vec![b'x'; LONGEST_LINE + 1];
        assert!(output.append(&long));
        assert!(output.pending.len() <= LONGEST_LINE);
    }

    #[test]
    fn at_the_shape_limit_the_shapes_start_over_and_a_repeat_is_still_no_progress() {
        let mut output = Written::stream();
        for index in 0..SHAPES_KEPT {
            output.shapes.insert(index.to_le_bytes().to_vec());
        }
        assert!(output.append(b"a line never seen\n"));
        assert_eq!(output.shapes.len(), 1);
        assert!(!output.append(b"a line never seen\n"));
    }

    #[test]
    fn a_nul_in_a_commands_output_skips_its_line_and_the_next_lines_still_count() {
        let mut output = Written::stream();
        assert!(!output.append(b"x\0header\n"));
        assert!(output.append(b"step one\n"));
        assert!(!output.append(b"\0\x01\x02\n"));
        assert!(output.append(b"step two done\n"));
        assert!(!output.append(b"step one\n"));
    }

    #[test]
    fn seconds_180_of_no_progress_ends_immediately() {
        let mut clock = StallClock::new(0.0, STALL_SECONDS).unwrap();
        assert!(!clock.observe(0.0, false, false));
        assert!(!clock.observe(179.0, false, false));
        assert_eq!(clock.remaining(179.0), 1.0);
        assert!(clock.observe(180.0, false, false));
        assert!(clock.observe(181.0, false, false));
    }

    #[test]
    fn progress_resets_idle_time_without_a_total_limit() {
        let mut clock = StallClock::new(0.0, STALL_SECONDS).unwrap();
        for step in 1..=10000 {
            assert!(!clock.observe(step as f64 * 179.0, true, false));
        }
        let last = 10000.0 * 179.0;
        assert!(!clock.observe(last + 179.0, false, false));
        assert!(clock.observe(last + 180.0, false, false));
        assert!(!clock.observe(last + 180.0, true, false));
        assert_eq!(clock.remaining(last + 180.0), 180.0);
    }

    #[test]
    fn repeating_status_exhausts_the_idle_limit() {
        let mut text = Written::default();
        let mut clock = StallClock::new(0.0, STALL_SECONDS).unwrap();
        assert!(!clock.observe(1.0, text.append(b"[2026.10.21-15.20.00:014][ 78]status frames=300\n"), false));
        assert!(!clock.observe(180.0, text.append(b"[2026.10.21-15.20.05:014][378]status frames=600\n"), false));
        assert!(clock.observe(181.0, text.append(b"[2026.10.21-15.20.10:014][678]status frames=900\n"), false));
        assert!(clock.observe(182.0, false, false));
    }

    #[test]
    fn a_nearer_live_watcher_defers_only_the_idle_judgement() {
        let mut clock = StallClock::new(0.0, STALL_SECONDS).unwrap();
        assert!(!clock.observe(179.0, false, true));
        assert_eq!(clock.remaining(179.0), 1.0);
        assert!(!clock.observe(180.0, false, true));
        assert!(!clock.observe(359.0, false, false));
        assert!(clock.observe(360.0, false, false));
        assert!(StallClock::new(0.0, 0.0).is_err());
    }

    #[test]
    fn a_declared_logs_baseline_bytes_are_not_progress() {
        let mut fs = Files::new();
        fs.write("/tree/log", "old\n");
        let mut log = LogFollower::new("/tree/log");
        log.baseline(&mut fs).unwrap();
        assert!(!drain(&mut log, &mut fs));
        assert_eq!(fs.bytes_read, 0);
        fs.append("/tree/log", "new step\n");
        assert!(drain(&mut log, &mut fs));
        assert_eq!(fs.bytes_read, 9);
        assert!(!drain(&mut log, &mut fs));
    }

    #[test]
    fn a_declared_log_that_appears_later_is_read_from_its_start() {
        let mut fs = Files::new();
        let mut log = LogFollower::new("/tree/late");
        log.baseline(&mut fs).unwrap();
        assert!(!drain(&mut log, &mut fs));
        fs.write("/tree/late", "first step\n");
        assert!(drain(&mut log, &mut fs));
    }

    #[test]
    fn a_replaced_or_truncated_log_is_read_again_without_crediting_old_shapes() {
        let mut fs = Files::new();
        let mut log = LogFollower::new("/tree/log");
        log.baseline(&mut fs).unwrap();
        fs.write("/tree/log", "status frames=1\nactive\n");
        assert!(drain(&mut log, &mut fs));
        fs.replace("/tree/log", "status frames=2\n");
        assert!(!drain(&mut log, &mut fs));
        fs.replace("/tree/log", "status frames=3\nrebuilt\n");
        assert!(drain(&mut log, &mut fs));
        fs.write("/tree/log", "a\n");
        assert!(drain(&mut log, &mut fs));
        fs.entries.remove("/tree/log");
        assert!(!drain(&mut log, &mut fs));
        fs.write("/tree/log", "active\n");
        assert!(!drain(&mut log, &mut fs));
    }

    #[test]
    fn a_log_is_read_one_budget_at_a_time() {
        let mut fs = Files::new();
        let mut log = LogFollower::new("/tree/log");
        log.baseline(&mut fs).unwrap();
        fs.write("/tree/log", "one\ntwo\nthree\n");
        assert_eq!(log.advance(&mut fs, 5).unwrap(), (true, true));
        assert_eq!(fs.bytes_read, 5);
        assert_eq!(log.advance(&mut fs, 5).unwrap(), (true, true));
        assert_eq!(log.advance(&mut fs, 5).unwrap(), (true, false));
        assert_eq!(fs.bytes_read, 14);
    }

    #[test]
    fn a_log_vanishing_between_stat_and_read_is_not_progress() {
        for rename in [false, true] {
            let mut fs = Files::new();
            let mut log = LogFollower::new("/tree/output");
            log.baseline(&mut fs).unwrap();
            fs.write("/tree/output", "temporary output\n");
            let reads = std::rc::Rc::new(std::cell::Cell::new(0));
            let counted = reads.clone();
            fs.before_read = Some(Box::new(move |entries, path| {
                counted.set(counted.get() + 1);
                let entry = entries.remove(path).unwrap();
                if rename {
                    entries.insert("/tree/moved".into(), entry);
                }
                Ok(())
            }));
            assert!(!drain(&mut log, &mut fs));
            assert_eq!(reads.get(), 1);
            fs.before_read = None;
            fs.write("/tree/output", "subsequent output\n");
            assert!(drain(&mut log, &mut fs));
        }
    }

    #[test]
    fn an_unreadable_log_is_an_error_not_silence() {
        let mut fs = Files::new();
        let mut log = LogFollower::new("/tree/output");
        log.baseline(&mut fs).unwrap();
        fs.write("/tree/output", "output\n");
        fs.before_read = Some(Box::new(|_, _| Err(Error::Unavailable("permission denied".into()))));
        let error = log.advance(&mut fs, 1 << 20).unwrap_err();
        assert_eq!(error.to_string(), "permission denied");
    }

    #[test]
    fn a_declared_directory_counts_new_names_and_reads_nothing() {
        let mut fs = Files::new();
        fs.directory("/tree/jobs");
        fs.write("/tree/jobs/old", "x");
        let mut jobs = DirectoryEntries::new("/tree/jobs");
        jobs.baseline(&mut fs).unwrap();
        assert!(!jobs.refresh(&mut fs).unwrap());
        fs.write("/tree/jobs/old", "rewritten");
        assert!(!jobs.refresh(&mut fs).unwrap());
        fs.directory("/tree/jobs/A1");
        fs.write("/tree/jobs/A1/input", "nested");
        assert!(jobs.refresh(&mut fs).unwrap());
        assert!(!jobs.refresh(&mut fs).unwrap());
        fs.entries.remove("/tree/jobs/old");
        assert!(!jobs.refresh(&mut fs).unwrap());
        fs.write("/tree/jobs/old", "back");
        assert!(jobs.refresh(&mut fs).unwrap());
        assert_eq!(fs.bytes_read, 0);
    }

    #[test]
    fn a_declared_directory_that_is_absent_holds_nothing_until_it_appears() {
        let mut fs = Files::new();
        let mut jobs = DirectoryEntries::new("/tree/later");
        jobs.baseline(&mut fs).unwrap();
        assert!(!jobs.refresh(&mut fs).unwrap());
        fs.directory("/tree/later");
        assert!(!jobs.refresh(&mut fs).unwrap());
        fs.write("/tree/later/job", "");
        assert!(jobs.refresh(&mut fs).unwrap());
    }

    #[test]
    fn declarations_take_complete_absolute_lines_only() {
        let mut parsed = Declarations::default();
        assert_eq!(parsed.append(b"log /a/b.log\ndir /a/jo").unwrap().len(), 1);
        let rest = parsed.append(b"bs\r\n\n  \n").unwrap();
        assert_eq!(rest, vec![Declaration { kind: DeclarationKind::Dir, path: "/a/jobs".into() }]);
        assert!(parsed.append(b"log relative.log\n").is_err());
        let mut other = Declarations::default();
        assert!(other.append(b"tree /a\n").is_err());
    }

    #[test]
    fn defer_registry_ignores_self_dead_invalid_and_vanished_entries() {
        let mut fs = Files::new();
        fs.directory("/tree/watchers");
        fs.write(&format!("/tree/watchers/{}", std::process::id()), "");
        fs.write("/tree/watchers/2147483647", "");
        fs.write("/tree/watchers/99999999999999999999999999999999999", "");
        fs.write("/tree/watchers/0", "");
        fs.write("/tree/watchers/note", "");
        assert!(!crate::native::judging_elsewhere(&mut fs, "/tree/watchers").unwrap());
        fs.write(&format!("/tree/watchers/{}", std::os::unix::process::parent_id()), "");
        assert!(crate::native::judging_elsewhere(&mut fs, "/tree/watchers").unwrap());
        fs.before_list = Some(Box::new(|entries, path| {
            entries.remove(path);
            Ok(())
        }));
        assert!(!crate::native::judging_elsewhere(&mut fs, "/tree/watchers").unwrap());
    }
}
