use crate::error::{Error, Result};
use crate::native::{Interest, Queue};
use crate::sha256::Sha256;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const HELD: &str = "CAIM_HELD";

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Project,
    Checkout,
    Machine,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Exclusive,
    Shared,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnBusy {
    #[default]
    Wait,
    Refuse,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Concurrency {
    pub group: String,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default)]
    pub on_busy: OnBusy,
}

impl Concurrency {
    fn named(group: &str) -> Self {
        Concurrency {
            group: group.into(),
            scope: Scope::default(),
            mode: Mode::default(),
            limit: None,
            on_busy: OnBusy::default(),
        }
    }

    pub fn check(&self) -> std::result::Result<(), String> {
        if !crate::tasks::valid_name(&self.group) {
            return Err(format!(
                "concurrency group {:?} is not a name: lowercase letters, digits and dashes",
                self.group
            ));
        }
        match (self.mode, self.limit) {
            (_, Some(0)) => Err(format!("concurrency group {}: limit must be at least 1", self.group)),
            (Mode::Exclusive, Some(_)) => {
                Err(format!("concurrency group {}: limit applies to shared holders only", self.group))
            }
            _ => Ok(()),
        }
    }
}

pub fn declared<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<Concurrency>, D::Error> {
    struct One;
    impl<'de> Visitor<'de> for One {
        type Value = Concurrency;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a group name or a [concurrency] table")
        }
        fn visit_str<E: de::Error>(self, group: &str) -> std::result::Result<Concurrency, E> {
            Ok(Concurrency::named(group))
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> std::result::Result<Concurrency, A::Error> {
            Concurrency::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }
    impl<'de> Deserialize<'de> for Wrapped {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
            deserializer.deserialize_any(One).map(Wrapped)
        }
    }
    struct Wrapped(Concurrency);
    struct Many;
    impl<'de> Visitor<'de> for Many {
        type Value = Vec<Concurrency>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a group name, a [concurrency] table, or a list of them")
        }
        fn visit_str<E: de::Error>(self, group: &str) -> std::result::Result<Self::Value, E> {
            Ok(vec![Concurrency::named(group)])
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> std::result::Result<Self::Value, A::Error> {
            Concurrency::deserialize(de::value::MapAccessDeserializer::new(map)).map(|one| vec![one])
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
            let mut all = Vec::new();
            while let Some(Wrapped(one)) = seq.next_element()? {
                all.push(one);
            }
            Ok(all)
        }
    }
    deserializer.deserialize_any(Many)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Holder {
    pub pid: i32,
    pub task: String,
    pub checkout: String,
    pub mode: Mode,
    pub since: String,
}

pub fn describe(holders: &[Holder]) -> String {
    if holders.is_empty() {
        return "a run that is just starting".into();
    }
    holders
        .iter()
        .map(|holder| format!("{} (pid {}) in {} since {}", holder.task, holder.pid, holder.checkout, holder.since))
        .collect::<Vec<_>>()
        .join(", ")
}

fn hash(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.finish().iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

fn scope_key(scope: Scope, project: &str, checkout: &Path) -> String {
    match scope {
        Scope::Machine => "machine".into(),
        Scope::Checkout => format!("checkout-{}", hash(checkout)),
        Scope::Project => format!("project-{project}"),
    }
}

fn lock_dir() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("CAIM_LOCK_DIR") {
        return Ok(PathBuf::from(directory));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cache = if cfg!(target_os = "macos") {
        home.map(|home| home.join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| home.map(|home| home.join(".cache")))
    };
    cache
        .map(|cache| cache.join("caim/locks"))
        .ok_or_else(|| Error::Unavailable("no HOME for the concurrency locks; set CAIM_LOCK_DIR".into()))
}

fn now() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn open(path: &Path) -> Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| Error::io(format!("open {}", path.display()), error))
}

fn try_lock(file: &File, operation: libc::c_int) -> Result<bool> {
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(Error::io("lock a concurrency group", error))
}

struct Intent {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for Intent {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct Group {
    concurrency: Concurrency,
    key: String,
    directory: PathBuf,
}

pub struct Held {
    pub key: String,
    locks: Vec<File>,
    record: Option<(PathBuf, File)>,
    bell: PathBuf,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some((path, _)) = self.record.take() {
            let _ = fs::remove_file(path);
        }
        self.locks.clear();
        if let Ok(bell) = open(&self.bell) {
            let _ = bell.set_modified(std::time::SystemTime::now());
        }
    }
}

impl Group {
    fn new(concurrency: &Concurrency, project: &str, checkout: &Path, base: &Path) -> Self {
        let scope = scope_key(concurrency.scope, project, checkout);
        Group {
            concurrency: concurrency.clone(),
            key: format!("{scope}/{}", concurrency.group),
            directory: base.join(scope).join(&concurrency.group),
        }
    }

    fn prepare(&self) -> Result<()> {
        let holders = self.directory.join("holders");
        fs::create_dir_all(&holders).map_err(|error| Error::io(format!("create {}", holders.display()), error))?;
        open(&self.directory.join("bell")).map(|_| ())
    }

    /// Announced before any lock is tried: a run killed between taking the group and writing its
    /// record still leaves a pid for a waiting run to watch.
    fn intend(&self) -> Result<Intent> {
        let path = self.directory.join(format!("holders/.{}", std::process::id()));
        let file = open(&path)?;
        try_lock(&file, libc::LOCK_EX)?;
        Ok(Intent { path, file: Some(file) })
    }

    fn intents(&self) -> Vec<i32> {
        let Ok(entries) = fs::read_dir(self.directory.join("holders")) else { return Vec::new() };
        let own = std::process::id() as i32;
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pid) = name.strip_prefix('.').and_then(|pid| pid.parse::<i32>().ok()) else { continue };
            if pid <= 0 || pid == own {
                continue;
            }
            if unsafe { libc::kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            found.push(pid);
        }
        found
    }

    fn take(&self, intent: &mut Intent, task: &str, checkout: &Path) -> Result<Option<Held>> {
        let gate = open(&self.directory.join("gate"))?;
        let mut locks = Vec::new();
        match self.concurrency.mode {
            Mode::Exclusive => {
                if !try_lock(&gate, libc::LOCK_EX)? {
                    return Ok(None);
                }
                locks.push(gate);
            }
            Mode::Shared => {
                if !try_lock(&gate, libc::LOCK_SH)? {
                    return Ok(None);
                }
                locks.push(gate);
                if let Some(limit) = self.concurrency.limit {
                    let mut slot = None;
                    for index in 0..limit {
                        let file = open(&self.directory.join(format!("slot-{index}")))?;
                        if try_lock(&file, libc::LOCK_EX)? {
                            slot = Some(file);
                            break;
                        }
                    }
                    let Some(slot) = slot else { return Ok(None) };
                    locks.push(slot);
                }
            }
        }
        let record = self.sign(intent, task, checkout)?;
        Ok(Some(Held { key: self.key.clone(), locks, record: Some(record), bell: self.directory.join("bell") }))
    }

    /// The locked intent becomes the record by a rename, so a reader never finds a record nobody holds.
    fn sign(&self, intent: &mut Intent, task: &str, checkout: &Path) -> Result<(PathBuf, File)> {
        let pid = std::process::id();
        let path = self.directory.join(format!("holders/{pid}"));
        let mut file =
            intent.file.take().ok_or_else(|| Error::Unavailable("a concurrency intent signed twice".into()))?;
        let holder = Holder {
            pid: pid as i32,
            task: task.into(),
            checkout: checkout.display().to_string(),
            mode: self.concurrency.mode,
            since: now(),
        };
        let text = serde_json::to_string(&holder).unwrap_or_default();
        file.set_len(0)
            .and_then(|_| file.write_all(format!("{text}\n").as_bytes()))
            .and_then(|_| fs::rename(&intent.path, &path))
            .map_err(|error| Error::io(format!("write {}", path.display()), error))?;
        Ok((path, file))
    }

    fn holders(&self) -> Vec<Holder> {
        let Ok(entries) = fs::read_dir(self.directory.join("holders")) else { return Vec::new() };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(mut file) = fs::File::open(entry.path()) else { continue };
            if try_lock(&file, libc::LOCK_SH).unwrap_or(false) {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let mut text = String::new();
            if file.read_to_string(&mut text).is_ok() {
                if let Ok(holder) = serde_json::from_str::<Holder>(text.trim()) {
                    found.push(holder);
                }
            }
        }
        found.sort_by_key(|holder| holder.pid);
        found
    }
}

pub enum Outcome {
    Held(Vec<Held>),
    Busy { group: Concurrency, holders: Vec<Holder> },
}

pub fn acquire(
    declared: &[Concurrency],
    task: &str,
    project: &str,
    checkout: &Path,
    held: &[String],
    waiting: &mut dyn FnMut(&Concurrency, &[Holder]),
) -> Result<Outcome> {
    if declared.is_empty() {
        return Ok(Outcome::Held(Vec::new()));
    }
    let checkout = fs::canonicalize(checkout).unwrap_or_else(|_| checkout.to_path_buf());
    let base = lock_dir()?;
    let mut groups: Vec<Group> = declared
        .iter()
        .map(|concurrency| Group::new(concurrency, project, &checkout, &base))
        .filter(|group| !held.contains(&group.key))
        .collect();
    groups.sort_by(|a, b| a.key.cmp(&b.key));
    groups.dedup_by(|a, b| a.key == b.key);
    let mut taken = Vec::new();
    let mut interrupted = None;
    for group in &groups {
        group.prepare()?;
        let mut intent = group.intend()?;
        if let Some(held) = group.take(&mut intent, task, &checkout)? {
            taken.push(held);
            continue;
        }
        if group.concurrency.on_busy == OnBusy::Refuse {
            return Ok(Outcome::Busy { group: group.concurrency.clone(), holders: group.holders() });
        }
        match wait(group, &mut intent, task, &checkout, waiting) {
            Ok(held) => taken.push(held),
            Err(error) => {
                interrupted = Some(error);
                break;
            }
        }
    }
    crate::native::restore_signals();
    match interrupted {
        Some(error) => Err(error),
        None => Ok(Outcome::Held(taken)),
    }
}

fn wait(
    group: &Group,
    intent: &mut Intent,
    task: &str,
    checkout: &Path,
    waiting: &mut dyn FnMut(&Concurrency, &[Holder]),
) -> Result<Held> {
    let mut told = false;
    loop {
        let queue = Queue::new()?;
        queue.termination()?;
        let bell_path = group.directory.join("bell");
        open(&bell_path)?;
        let _bell = queue
            .watch(&bell_path, true, Interest::Touched)
            .map_err(|error| Error::io(format!("open {}", bell_path.display()), error))?;
        let _holders = queue
            .watch(&group.directory.join("holders"), true, Interest::Written)
            .map_err(|error| Error::io("open the holders of a concurrency group", error))?;
        let holders = group.holders();
        for pid in holders.iter().map(|holder| holder.pid).chain(group.intents()) {
            queue.process(pid)?;
        }
        // Registered first, then tried again, so a release between the two is not missed.
        if let Some(held) = group.take(intent, task, checkout)? {
            return Ok(held);
        }
        if !told {
            waiting(&group.concurrency, &holders);
            told = true;
        }
        queue.wait(None)?;
    }
}
