use super::events::{Event, Interest, Token};
use super::os::{Queue, Watch};
use super::watch::READ_BUDGET;
use crate::error::{Error, Result, missing};
use crate::progress::{
    Appended, DeclarationKind, Declarations, DirectoryEntries, FileState, FileSystem, Kind, LogFollower,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

pub struct PosixFiles;

impl FileSystem for PosixFiles {
    fn stat(&mut self, path: &str) -> Result<Option<FileState>> {
        match fs::symlink_metadata(path) {
            Ok(info) => {
                let kind = if info.file_type().is_file() {
                    Kind::File
                } else if info.file_type().is_dir() {
                    Kind::Directory
                } else {
                    Kind::Other
                };
                Ok(Some(FileState { device: info.dev(), inode: info.ino(), size: info.size(), kind }))
            }
            Err(error) if missing(&error) => Ok(None),
            Err(error) => Err(Error::io(format!("stat {path}"), error)),
        }
    }

    fn list(&mut self, path: &str) -> Result<Option<Vec<String>>> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(Error::io(format!("list {path}"), error)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(format!("read directory {path}"), error))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(Some(names))
    }

    fn read(&mut self, path: &str, offset: u64, count: u64) -> Result<Option<Vec<u8>>> {
        let file = match fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
        {
            Ok(file) => file,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(Error::io(format!("open {path}"), error)),
        };
        let info = file.metadata().map_err(|error| Error::io(format!("fstat {path}"), error))?;
        if !info.file_type().is_file() {
            return Ok(None);
        }
        let mut bytes = vec![0; count as usize];
        let size = loop {
            match file.read_at(&mut bytes, offset) {
                Ok(size) => break size,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::io(format!("read {path}"), error)),
            }
        };
        bytes.truncate(size);
        Ok(Some(bytes))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SourceKind {
    Log,
    Directory,
    Declarations,
}

struct Source {
    kind: SourceKind,
    path: String,
    log: Option<LogFollower>,
    entries: Option<DirectoryEntries>,
    appended: Option<Appended>,
    parsed: Declarations,
    target: Option<Watch>,
    target_id: (u64, u64),
    anchor: Option<Watch>,
    anchor_path: String,
}

impl Source {
    fn new(kind: SourceKind, path: &str) -> Self {
        Source {
            kind,
            path: path.to_string(),
            log: None,
            entries: None,
            appended: None,
            parsed: Declarations::default(),
            target: None,
            target_id: (0, 0),
            anchor: None,
            anchor_path: String::new(),
        }
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        None | Some(0) => "/".to_string(),
        Some(slash) => path[..slash].to_string(),
    }
}

pub struct Sources {
    filesystem: PosixFiles,
    sources: Vec<Source>,
    owners: HashMap<Token, usize>,
    declared: HashSet<(SourceKind, String)>,
}

impl Default for Sources {
    fn default() -> Self {
        Self::new()
    }
}

impl Sources {
    pub fn new() -> Self {
        Sources { filesystem: PosixFiles, sources: Vec::new(), owners: HashMap::new(), declared: HashSet::new() }
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    fn add(&mut self, queue: &Queue, source: Source) -> Result<usize> {
        let id = self.sources.len();
        self.sources.push(source);
        self.attach(queue, id)?;
        Ok(id)
    }

    pub fn log(&mut self, queue: &Queue, path: &str) -> Result<()> {
        if !self.declared.insert((SourceKind::Log, path.to_string())) {
            return Ok(());
        }
        let mut source = Source::new(SourceKind::Log, path);
        let mut log = LogFollower::new(path);
        log.baseline(&mut self.filesystem)?;
        source.log = Some(log);
        self.add(queue, source).map(|_| ())
    }

    pub fn directory(&mut self, queue: &Queue, path: &str) -> Result<()> {
        if !self.declared.insert((SourceKind::Directory, path.to_string())) {
            return Ok(());
        }
        let mut source = Source::new(SourceKind::Directory, path);
        let mut entries = DirectoryEntries::new(path);
        entries.baseline(&mut self.filesystem)?;
        source.entries = Some(entries);
        self.add(queue, source).map(|_| ())
    }

    pub fn declarations(&mut self, queue: &Queue, path: &str) -> Result<()> {
        if !self.declared.insert((SourceKind::Declarations, path.to_string())) {
            return Ok(());
        }
        let mut source = Source::new(SourceKind::Declarations, path);
        // Every declaration counts, including those written before the watch began.
        source.appended = Some(Appended::new(path));
        let id = self.add(queue, source)?;
        while self.service(queue, id, READ_BUDGET)?.1 {}
        Ok(())
    }

    pub fn owner(&self, event: &Event) -> Option<usize> {
        match event {
            Event::Changed(token) => self.owners.get(token).copied(),
            _ => None,
        }
    }

    fn attach(&mut self, queue: &Queue, id: usize) -> Result<()> {
        let source = &mut self.sources[id];
        let present = match fs::symlink_metadata(&source.path) {
            Ok(info) => Some((info.dev(), info.ino())),
            Err(error) if missing(&error) => None,
            Err(error) => return Err(Error::io(format!("stat {}", source.path), error)),
        };
        if source.target.is_some() && present != Some(source.target_id) {
            if let Some(watch) = source.target.take() {
                self.owners.remove(&watch.token());
            }
        }
        if let (Some(identity), None) = (present, &source.target) {
            match queue.watch(Path::new(&source.path), false, Interest::Source) {
                Ok(watch) => {
                    self.owners.insert(watch.token(), id);
                    source.target = Some(watch);
                    source.target_id = identity;
                }
                Err(error) if missing(&error) => {}
                Err(error) => return Err(Error::io(format!("open {}", source.path), error)),
            }
        }
        if source.target.is_some() {
            if let Some(watch) = source.anchor.take() {
                self.owners.remove(&watch.token());
            }
            source.anchor_path.clear();
            return Ok(());
        }
        let mut ancestor = parent_of(&source.path);
        loop {
            match fs::symlink_metadata(&ancestor) {
                Ok(info) if info.is_dir() => break,
                Ok(_) => {}
                Err(error) if missing(&error) => {}
                Err(error) => return Err(Error::io(format!("stat {ancestor}"), error)),
            }
            ancestor = parent_of(&ancestor);
        }
        if ancestor == source.anchor_path && source.anchor.is_some() {
            return Ok(());
        }
        if let Some(watch) = source.anchor.take() {
            self.owners.remove(&watch.token());
        }
        let watch = queue
            .watch(Path::new(&ancestor), true, Interest::Ancestor)
            .map_err(|error| Error::io(format!("open {ancestor}"), error))?;
        self.owners.insert(watch.token(), id);
        source.anchor = Some(watch);
        source.anchor_path = ancestor;
        Ok(())
    }

    pub fn service(&mut self, queue: &Queue, id: usize, budget: u64) -> Result<(bool, bool)> {
        self.attach(queue, id)?;
        let source = &mut self.sources[id];
        match source.kind {
            SourceKind::Log => source.log.as_mut().expect("log source").advance(&mut self.filesystem, budget),
            SourceKind::Directory => {
                Ok((source.entries.as_mut().expect("directory source").refresh(&mut self.filesystem)?, false))
            }
            SourceKind::Declarations => {
                let chunk =
                    source.appended.as_mut().expect("declarations source").next(&mut self.filesystem, budget)?;
                if chunk.restarted {
                    source.parsed.restart();
                }
                let more = chunk.more;
                let found = source.parsed.append(&chunk.bytes)?;
                let before = self.sources.len();
                for declaration in found {
                    match declaration.kind {
                        DeclarationKind::Log => self.log(queue, &declaration.path)?,
                        DeclarationKind::Dir => self.directory(queue, &declaration.path)?,
                    }
                }
                Ok((self.sources.len() != before, more))
            }
        }
    }
}
