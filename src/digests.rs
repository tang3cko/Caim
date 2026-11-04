use crate::error::{Error, Result, FAIL, PASS};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

pub const MODES: &[&str] = &["hash-check", "hash"];

#[cfg(not(target_os = "macos"))]
use crate::sha256::Sha256;
#[cfg(target_os = "macos")]
use commoncrypto::Sha256;

#[cfg(target_os = "macos")]
mod commoncrypto {
    #[repr(C)]
    struct Context {
        count: [u32; 2],
        hash: [u32; 8],
        wbuf: [u32; 16],
    }

    extern "C" {
        fn CC_SHA256_Init(context: *mut Context) -> i32;
        fn CC_SHA256_Update(context: *mut Context, data: *const u8, length: u32) -> i32;
        fn CC_SHA256_Final(digest: *mut u8, context: *mut Context) -> i32;
    }

    pub struct Sha256(Context);

    impl Sha256 {
        pub fn new() -> Self {
            let mut context = Context { count: [0; 2], hash: [0; 8], wbuf: [0; 16] };
            unsafe { CC_SHA256_Init(&mut context) };
            Sha256(context)
        }

        pub fn update(&mut self, data: &[u8]) {
            for piece in data.chunks(u32::MAX as usize) {
                unsafe { CC_SHA256_Update(&mut self.0, piece.as_ptr(), piece.len() as u32) };
            }
        }

        pub fn finish(mut self) -> [u8; 32] {
            let mut digest = [0u8; 32];
            unsafe { CC_SHA256_Final(digest.as_mut_ptr(), &mut self.0) };
            digest
        }
    }

    #[cfg(test)]
    #[test]
    fn matches_the_portable_sha256() {
        for length in [0usize, 1, 55, 56, 63, 64, 65, 1000, 100_000] {
            let data: Vec<u8> = (0..length).map(|i| (i * 131 % 251) as u8).collect();
            let mut system = Sha256::new();
            system.update(&data);
            let mut portable = crate::sha256::Sha256::new();
            portable.update(&data);
            assert_eq!(system.finish(), portable.finish(), "length {length}");
        }
    }
}

const CHUNK: usize = 1 << 20;

fn sha256(path: &str, buffer: &mut [u8]) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    loop {
        let read = match file.read(buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hash.update(&buffer[..read]);
    }
    Ok(hash.finish().iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hash_all(paths: &[String]) -> Vec<io::Result<String>> {
    let workers = thread::available_parallelism().map_or(4, |count| count.get()).min(paths.len().max(1));
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<io::Result<String>>> = (0..paths.len()).map(|_| None).collect();
    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut buffer = vec![0u8; CHUNK];
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= paths.len() {
                            break;
                        }
                        done.push((index, sha256(&paths[index], &mut buffer)));
                    }
                    done
                })
            })
            .collect();
        for handle in handles {
            for (index, result) in handle.join().expect("a hashing thread panicked") {
                results[index] = Some(result);
            }
        }
    });
    results.into_iter().map(|result| result.expect("every path was hashed")).collect()
}

struct Line {
    list: usize,
    digest: String,
    path: String,
}

fn read_list(list: &str, index: usize, lines: &mut Vec<Line>) -> Result<()> {
    let text = std::fs::read_to_string(list).map_err(|error| Error::io(format!("cannot read {list}"), error))?;
    for (number, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let parsed = line.split_once("  ").filter(|(digest, path)| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) && !path.is_empty()
        });
        let Some((digest, path)) = parsed else {
            return Err(Error::Usage(format!("{list}:{}: not a `shasum -a 256` line", number + 1)));
        };
        lines.push(Line { list: index, digest: digest.to_ascii_lowercase(), path: path.to_string() });
    }
    Ok(())
}

fn hash_check(lists: &[String]) -> Result<i32> {
    if lists.is_empty() {
        return Err(Error::Usage("usage: caim hash-check LIST...".into()));
    }
    let mut lines = Vec::new();
    for (index, list) in lists.iter().enumerate() {
        read_list(list, index, &mut lines)?;
    }
    let mut position: HashMap<&str, usize> = HashMap::new();
    let mut paths: Vec<String> = Vec::new();
    for line in &lines {
        position.entry(&line.path).or_insert_with(|| {
            paths.push(line.path.clone());
            paths.len() - 1
        });
    }
    let digests = hash_all(&paths);
    let mut output = io::stdout().lock();
    let mut stale = false;
    for line in &lines {
        let current = &digests[position[line.path.as_str()]];
        if current.as_ref().map_or(true, |digest| *digest != line.digest) {
            stale = true;
            writeln!(output, "{}\t{}", lists[line.list], line.path).map_err(|error| Error::io("stdout", error))?;
        }
    }
    Ok(if stale { FAIL } else { PASS })
}

fn hash(arguments: &[String]) -> Result<i32> {
    if !arguments.is_empty() {
        return Err(Error::Usage("usage: caim hash < NUL-separated paths".into()));
    }
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input).map_err(|error| Error::io("stdin", error))?;
    let paths: Vec<String> = input
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).map_err(|_| Error::Usage("a path is not UTF-8".into())))
        .collect::<Result<_>>()?;
    let digests = hash_all(&paths);
    let mut text = String::new();
    for (path, digest) in paths.iter().zip(&digests) {
        match digest {
            Ok(digest) => text.push_str(&format!("{digest}  {path}\n")),
            Err(error) => {
                eprintln!("caim hash: {path}: {error}");
                return Ok(FAIL);
            }
        }
    }
    io::stdout().lock().write_all(text.as_bytes()).map_err(|error| Error::io("stdout", error))?;
    Ok(PASS)
}

pub fn dispatch(arguments: &[String]) -> Result<i32> {
    match arguments[0].as_str() {
        "hash-check" => hash_check(&arguments[1..]),
        _ => hash(&arguments[1..]),
    }
}
