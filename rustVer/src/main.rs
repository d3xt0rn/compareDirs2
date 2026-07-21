// compareDirs2.rs
//
// Rust port of compareDirs2.sh
//
// Compare all files in DIR1 with DIR2 recursively.
// Styled after OpenRC / emerge.

use clap::Parser;
use digest::Digest;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

//////////////////////////////
// Colors (OpenRC style)    //
//////////////////////////////
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[1;32m";
const RED: &str = "\x1b[1;31m";
const BLUE: &str = "\x1b[1;34m";
const YELLOW: &str = "\x1b[1;33m"; // used as "orange"
const CYAN: &str = "\x1b[1;36m";
const WHITE: &str = "\x1b[1;37m";

const SPINNER: [&str; 4] = ["/", "-", "\\", "|"];

//////////////////////////////
// CLI                      //
//////////////////////////////

#[derive(Parser, Debug)]
#[command(
    name = "compare_dirs",
    version,
    about = "Compare all files in DIR1 with DIR2 recursively.",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct RawArgs {
    #[arg(short = 'x', long = "hash")]
    hash: bool,

    #[arg(short = 'a', long = "algo", default_value = "sha256sum")]
    algo: String,

    #[arg(long = "min-size")]
    min_size: Option<String>,

    #[arg(long = "max-size")]
    max_size: Option<String>,

    #[arg(long = "max-find-time")]
    max_find_time: Option<String>,

    #[arg(short = 'r', long = "find-renamed")]
    find_renamed: bool,

    /// Show a single-line emerge-style progress bar instead of a per-file spinner
    #[arg(short = 'p', long = "progress")]
    progress: bool,

    /// Number of parallel worker threads (0 = auto/num CPUs, default: 1)
    #[arg(short = 'j', long = "jobs")]
    jobs: Option<usize>,

    /// Only print errors and the final summary
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Always print full paths instead of paths relative to DIR1
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Follow symlinks while walking directories
    #[arg(short = 'L', long = "follow-symlinks")]
    follow_symlinks: bool,

    /// Skip paths whose relative name contains SUBSTR (repeatable)
    #[arg(long = "exclude", value_name = "SUBSTR")]
    exclude: Vec<String>,

    /// Don't check anything, only list what would be checked
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Disable the on-disk hash cache used to speed up --find-renamed
    #[arg(long = "no-hash-cache")]
    no_hash_cache: bool,

    /// Custom path for the hash-cache temp file (default: /tmp/compareDirs2-hashes-<pid>.tsv)
    #[arg(long = "cache-file", value_name = "PATH")]
    cache_file: Option<String>,

    /// Keep the hash-cache temp file after the run instead of deleting it
    #[arg(long = "keep-cache-file")]
    keep_cache_file: bool,

    /// Print elapsed time and throughput in the final summary
    #[arg(long = "stats")]
    stats: bool,

    #[arg(short = 'h', long = "help", action = clap::ArgAction::SetTrue)]
    help: bool,

    #[arg(short = 'V', long = "version", action = clap::ArgAction::SetTrue)]
    version: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    positional: Vec<String>,
}

fn usage(prog: &str) {
    println!(
        r#"Usage: {prog} [OPTIONS] DIR1 DIR2

Options:
  -x, --hash              Compare files by hash instead of byte-for-byte cmp
  -a, --algo ALGO         Hash algorithm (default: sha256sum). One of:
                           md5sum, sha1sum, sha256sum, sha512sum,
                           sha3-256sum, sha3-512sum, b2sum, b3sum,
                           xxh32sum, xxh64sum, xxh3sum, crc32sum
      --min-size SIZE      Only hash-compare files >= SIZE (e.g., 500B, 2KB, 10MiB, 1GB)
                           (smaller files use cmp)
      --max-size SIZE      Only hash-compare files <= SIZE (e.g., 100MB, 2GiB)
                           (larger files use cmp)
      --max-find-time T    Max time allowed for find search per file (e.g., 5s, 2m, 1h)
  -r, --find-renamed       If a file is missing in DIR2 — search DIR2 for a
                           file with identical content/hash (rename detection)
  -p, --progress           Show a single-line emerge-style progress bar
  -j, --jobs N             Parallel worker threads (0 = auto, default: 1)
  -q, --quiet              Only print errors and the final summary
  -v, --verbose            Always print full paths instead of relative paths
  -L, --follow-symlinks    Follow symlinks while walking directories
      --exclude SUBSTR     Skip paths containing SUBSTR (repeatable)
      --dry-run            List what would be checked, then exit
      --no-hash-cache      Disable the on-disk hash cache used by --find-renamed
      --cache-file PATH    Custom path for the hash-cache temp file
                           (default: /tmp/compareDirs2-hashes-<pid>.tsv)
      --keep-cache-file    Keep the hash-cache temp file after the run
      --stats              Print elapsed time and throughput in the summary
  -h, --help               Show this help
  -V, --version            Show version information
"#
    );
}

//////////////////////////////
// Parsers (Size & Time)    //
//////////////////////////////

// Converts human-readable size to bytes.
fn parse_size(val: &str) -> Result<u64, String> {
    let clean = val.trim().to_lowercase();

    if clean.chars().all(|c| c.is_ascii_digit()) && !clean.is_empty() {
        return clean
            .parse::<u64>()
            .map_err(|_| format!("Error: Invalid size format '{val}'"));
    }

    let split_at = clean.find(|c: char| !c.is_ascii_digit());
    if let Some(idx) = split_at {
        if idx == 0 {
            return Err(format!("Error: Invalid size format '{val}'"));
        }
        let (num_s, unit) = clean.split_at(idx);
        if !unit.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(format!("Error: Invalid size format '{val}'"));
        }
        let num: u64 = num_s
            .parse()
            .map_err(|_| format!("Error: Invalid size format '{val}'"))?;

        // NOTE: bugfix vs. the original bash script — tb/pb used to use
        // 10^11 / 10^14 (a copy-paste bug). Now correct decimal SI factors.
        let factor: u64 = match unit {
            "b" => 1,
            "k" | "kb" => 1_000,
            "m" | "mb" => 1_000_000,
            "g" | "gb" => 1_000_000_000,
            "t" | "tb" => 1_000_000_000_000,
            "p" | "pb" => 1_000_000_000_000_000,
            "ki" | "kib" => 1_024,
            "mi" | "mib" => 1_048_576,
            "gi" | "gib" => 1_073_741_824,
            "ti" | "tib" => 1_099_511_627_776,
            "pi" | "pib" => 1_125_899_906_842_624,
            _ => {
                return Err(format!(
                    "Error: Unknown size suffix '{unit}' in argument '{val}'"
                ))
            }
        };
        num.checked_mul(factor)
            .ok_or_else(|| format!("Error: Size '{val}' overflows u64"))
    } else {
        Err(format!("Error: Invalid size format '{val}'"))
    }
}

// Converts human-readable time to seconds
fn parse_time(val: &str) -> Result<u64, String> {
    let clean = val.trim().to_lowercase();

    if clean.chars().all(|c| c.is_ascii_digit()) && !clean.is_empty() {
        return clean
            .parse::<u64>()
            .map_err(|_| format!("Error: Invalid time format '{val}'"));
    }

    let split_at = clean.find(|c: char| !c.is_ascii_digit());
    if let Some(idx) = split_at {
        if idx == 0 {
            return Err(format!("Error: Invalid time format '{val}'"));
        }
        let (num_s, unit) = clean.split_at(idx);
        if !unit.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(format!("Error: Invalid time format '{val}'"));
        }
        let num: u64 = num_s
            .parse()
            .map_err(|_| format!("Error: Invalid time format '{val}'"))?;

        let factor: u64 = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            "w" => 604_800,
            _ => {
                return Err(format!(
                    "Error: Unknown time suffix '{unit}' in argument '{val}'"
                ))
            }
        };
        num.checked_mul(factor)
            .ok_or_else(|| format!("Error: Time '{val}' overflows u64"))
    } else {
        Err(format!("Error: Invalid time format '{val}'"))
    }
}

//////////////////////////////
// Hash cache (/tmp)         //
//////////////////////////////
//
// find_renamed() re-walks the whole of DIR2 for *every* missing file.
// Without a cache that means every file in DIR2 gets hashed again from
// scratch for each missing file — extremely wasteful when several files
// are missing. HashCache memoizes hash_of() results in memory and mirrors
// them to a temp file on disk (so state can be inspected/reused), and is
// removed on normal exit or interruption unless --keep-cache-file is set.
struct HashCache {
    enabled: bool,
    map: Mutex<std::collections::HashMap<PathBuf, String>>,
    file: Mutex<Option<File>>,
}

impl HashCache {
    fn new(enabled: bool, path: Option<&Path>) -> Self {
        let file = if enabled {
            path.and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            })
        } else {
            None
        };
        HashCache {
            enabled,
            map: Mutex::new(std::collections::HashMap::new()),
            file: Mutex::new(file),
        }
    }

    /// Returns the hash of `path`, using the cache when enabled.
    fn hash_of_cached(&self, path: &Path, algo: HashAlgo) -> io::Result<String> {
        if self.enabled {
            if let Some(h) = self.map.lock().unwrap().get(path) {
                return Ok(h.clone());
            }
        }
        let h = hash_of(path, algo)?;
        if self.enabled {
            self.map
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), h.clone());
            if let Some(f) = self.file.lock().unwrap().as_mut() {
                let _ = writeln!(f, "{}\t{}", path.display(), h);
            }
        }
        Ok(h)
    }
}

// Global handle to the cache temp-file path so signal/exit handlers can
// clean it up regardless of where in the program we are.
static CACHE_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static KEEP_CACHE_FILE: AtomicBool = AtomicBool::new(false);

fn register_cache_path(path: PathBuf) {
    *CACHE_PATH.lock().unwrap() = Some(path);
}

fn cleanup_cache_file() {
    if KEEP_CACHE_FILE.load(Ordering::SeqCst) {
        return;
    }
    if let Some(p) = CACHE_PATH.lock().unwrap().take() {
        let _ = std::fs::remove_file(p);
    }
}

//////////////////////////////
// Hashing                  //
//////////////////////////////

#[derive(Clone, Copy, Debug, PartialEq)]
enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Sha3_256,
    Sha3_512,
    Blake2b,
    Blake3,
    Xxh32,
    Xxh64,
    Xxh3,
    Crc32,
}

impl HashAlgo {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "md5sum" | "md5" => Some(HashAlgo::Md5),
            "sha1sum" | "sha1" => Some(HashAlgo::Sha1),
            "sha256sum" | "sha256" => Some(HashAlgo::Sha256),
            "sha512sum" | "sha512" => Some(HashAlgo::Sha512),
            "sha3-256sum" | "sha3-256" => Some(HashAlgo::Sha3_256),
            "sha3-512sum" | "sha3-512" => Some(HashAlgo::Sha3_512),
            "b2sum" | "blake2b" => Some(HashAlgo::Blake2b),
            "b3sum" | "blake3" => Some(HashAlgo::Blake3),
            "xxh32sum" | "xxh32" => Some(HashAlgo::Xxh32),
            "xxh64sum" | "xxh64" => Some(HashAlgo::Xxh64),
            "xxh3sum" | "xxh3" => Some(HashAlgo::Xxh3),
            "crc32sum" | "crc32" => Some(HashAlgo::Crc32),
            _ => None,
        }
    }
}

const ALLOWED_ALGOS: [&str; 12] = [
    "md5sum",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "sha3-256sum",
    "sha3-512sum",
    "b2sum",
    "b3sum",
    "xxh32sum",
    "xxh64sum",
    "xxh3sum",
    "crc32sum",
];

fn hash_of(path: &Path, algo: HashAlgo) -> io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; 65536];

    macro_rules! digest_loop {
        ($hasher:expr) => {{
            let mut hasher = $hasher;
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            format!("{:x}", hasher.finalize())
        }};
    }

    let hex = match algo {
        HashAlgo::Md5 => digest_loop!(md5::Md5::new()),
        HashAlgo::Sha1 => digest_loop!(sha1::Sha1::new()),
        HashAlgo::Sha256 => digest_loop!(sha2::Sha256::new()),
        HashAlgo::Sha512 => digest_loop!(sha2::Sha512::new()),
        HashAlgo::Sha3_256 => digest_loop!(sha3::Sha3_256::new()),
        HashAlgo::Sha3_512 => digest_loop!(sha3::Sha3_512::new()),
        HashAlgo::Blake2b => digest_loop!(blake2::Blake2b512::new()),
        HashAlgo::Blake3 => {
            let mut hasher = blake3::Hasher::new();
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            hasher.finalize().to_hex().to_string()
        }
        HashAlgo::Xxh32 => {
            let mut hasher = xxhash_rust::xxh32::Xxh32::new(0);
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            format!("{:08x}", hasher.digest())
        }
        HashAlgo::Xxh64 => {
            let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            format!("{:016x}", hasher.digest())
        }
        HashAlgo::Xxh3 => {
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            format!("{:016x}", hasher.digest())
        }
        HashAlgo::Crc32 => {
            let mut hasher = crc32fast::Hasher::new();
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            format!("{:08x}", hasher.finalize())
        }
    };
    Ok(hex)
}

fn filesize(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn files_equal_bytes(f1: &Path, f2: &Path) -> io::Result<bool> {
    let m1 = std::fs::metadata(f1)?;
    let m2 = std::fs::metadata(f2)?;
    if m1.len() != m2.len() {
        return Ok(false);
    }
    let mut r1 = BufReader::new(File::open(f1)?);
    let mut r2 = BufReader::new(File::open(f2)?);
    let mut b1 = [0u8; 65536];
    let mut b2 = [0u8; 65536];
    loop {
        let n1 = r1.read(&mut b1)?;
        let n2 = r2.read(&mut b2)?;
        if n1 != n2 {
            return Ok(false);
        }
        if n1 == 0 {
            return Ok(true);
        }
        if b1[..n1] != b2[..n2] {
            return Ok(false);
        }
    }
}

//////////////////////////////
// Terminal / Status column //
//////////////////////////////

fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .unwrap_or(80)
}

struct Status {
    last_lines: usize,
}

impl Status {
    fn new() -> Self {
        Status { last_lines: 1 }
    }

    fn clear(&mut self) {
        let mut out = io::stdout();
        if self.last_lines > 1 {
            let _ = write!(out, "\x1b[{}A", self.last_lines - 1);
        }
        let _ = write!(out, "\r\x1b[J");
        let _ = out.flush();
    }

    fn finish_line(&mut self) {
        println!();
        self.last_lines = 1;
    }

    fn status_line(
        &mut self,
        prefix: &str,
        pcolor: &str,
        msg_in: &str,
        status: &str,
        scolor: &str,
    ) {
        let term_w = term_width();
        let status_len = status.chars().count();
        let fixed = status_len + 6;
        let mut msg = msg_in.to_string();
        let mut msglen = msg.chars().count();

        self.clear();
        let mut out = io::stdout();

        if term_w >= msglen + fixed + 2 {
            let pad = (term_w as i64 - msglen as i64 - fixed as i64).max(1) as usize;
            let _ = write!(
                out,
                "{pcolor}{prefix}{RESET} {msg}{pad}{BLUE}[{RESET} {scolor}{status}{RESET} {BLUE}]{RESET}",
                pad = " ".repeat(pad),
            );
            self.last_lines = 1;
        } else if term_w >= msglen + 2 {
            let _ = writeln!(out, "{pcolor}{prefix}{RESET} {msg}");
            let _ = write!(
                out,
                "    {BLUE}[{RESET} {scolor}{status}{RESET} {BLUE}]{RESET}"
            );
            self.last_lines = 2;
        } else {
            let maxmsg = ((term_w as i64) - (fixed as i64) - 2).max(5) as usize;
            if msglen > maxmsg {
                let chars: Vec<char> = msg.chars().collect();
                let tail_len = maxmsg.saturating_sub(3).max(1);
                let tail: String = chars[chars.len().saturating_sub(tail_len)..]
                    .iter()
                    .collect();
                msg = format!("...{tail}");
                msglen = msg.chars().count();
            }
            let pad = (term_w as i64 - msglen as i64 - fixed as i64).max(1) as usize;
            let _ = write!(
                out,
                "{pcolor}{prefix}{RESET} {msg}{pad}{BLUE}[{RESET} {scolor}{status}{RESET} {BLUE}]{RESET}",
                pad = " ".repeat(pad),
            );
            self.last_lines = 1;
        }
        let _ = out.flush();
    }

    /// Emerge-style single-line progress bar:
    /// " * Checking (12 of 340) [#####-------] 35%  some/file.txt"
    fn progress_line(&mut self, index: usize, total: usize, err: usize, file: &str) {
        let term_w = term_width();
        self.clear();
        let mut out = io::stdout();

        let done = index.min(total);
        let pct = if total == 0 {
            100
        } else {
            (done * 100) / total
        };

        let counter = format!("({done} of {total})");
        let pct_s = format!("{pct:>3}%");
        let errs = format!(" err:{err}");
        let reserved = 3 + counter.len() + 3 + pct_s.len() + errs.len() + 4;
        let bar_width = (term_w.saturating_sub(reserved)).clamp(10, 40);
        let filled = if total == 0 {
            bar_width
        } else {
            (bar_width * done) / total
        };
        let bar: String = "#".repeat(filled) + &"-".repeat(bar_width - filled);

        let name_budget = term_w.saturating_sub(reserved + bar_width + 3);
        let mut name = file.to_string();
        if name.chars().count() > name_budget && name_budget > 3 {
            let chars: Vec<char> = name.chars().collect();
            let tail_len = name_budget.saturating_sub(3);
            let tail: String = chars[chars.len().saturating_sub(tail_len)..]
                .iter()
                .collect();
            name = format!("...{tail}");
        }

        let ecolor = if err > 0 { RED } else { GREEN };
        let _ = write!(
            out,
            "{GREEN}*{RESET} Checking {counter} {BLUE}[{RESET}{CYAN}{bar}{RESET}{BLUE}]{RESET} {pct_s} {ecolor}{errs}{RESET}  {name}"
        );
        self.last_lines = 1;
        let _ = out.flush();
    }
}

//////////////////////////////
// Spinner                  //
//////////////////////////////

struct SpinnerHandle {
    stop_flag: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

fn start_spinner(
    status: Arc<Mutex<Status>>,
    msg: String,
    color: &'static str,
    finding: bool,
) -> SpinnerHandle {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();
    let join = thread::spawn(move || {
        let mut i = 0usize;
        while !stop_flag_thread.load(Ordering::Relaxed) {
            if PAUSED.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(80));
                continue;
            }
            let c = SPINNER[i % SPINNER.len()];
            let label = if finding {
                format!("finding {c}")
            } else {
                c.to_string()
            };
            {
                let mut st = status.lock().unwrap();
                st.status_line(" ", RESET, &msg, &label, color);
            }
            i += 1;
            thread::sleep(Duration::from_millis(80));
        }
    });
    SpinnerHandle {
        stop_flag,
        join: Some(join),
    }
}

fn stop_spinner(mut handle: SpinnerHandle) {
    handle.stop_flag.store(true, Ordering::Relaxed);
    if let Some(j) = handle.join.take() {
        let _ = j.join();
    }
}

//////////////////////////////
// Global counters (for the //
// Ctrl+C / Ctrl+Z handler) //
//////////////////////////////

static OK_COUNT: AtomicUsize = AtomicUsize::new(0);
static ERR_COUNT: AtomicUsize = AtomicUsize::new(0);
static INDEX: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
// Set while the process is (about to be) suspended via Ctrl+Z, so worker /
// spinner threads stop touching the terminal instead of racing the shell
// when the job is later resumed with `fg`.
static PAUSED: AtomicBool = AtomicBool::new(false);

//////////////////////////////
// Compare logic             //
//////////////////////////////

#[derive(Clone)]
struct CompareOpts {
    hash_mode: bool,
    algo: HashAlgo,
    min_size: u64,
    max_size: u64,
}

fn compare_files(f1: &Path, f2: &Path, opts: &CompareOpts) -> bool {
    if opts.hash_mode {
        let size = filesize(f1);
        let mut use_hash = true;
        if opts.min_size > 0 && size < opts.min_size {
            use_hash = false;
        }
        if opts.max_size > 0 && size > opts.max_size {
            use_hash = false;
        }
        if use_hash {
            let h1 = hash_of(f1, opts.algo).unwrap_or_default();
            let h2 = hash_of(f2, opts.algo).unwrap_or_default();
            return h1 == h2 && !h1.is_empty();
        }
    }
    files_equal_bytes(f1, f2).unwrap_or(false)
}

fn find_renamed(
    dir2: &Path,
    f1: &Path,
    opts: &CompareOpts,
    max_find_time: u64,
    cache: &HashCache,
) -> FindResult {
    let start = Instant::now();
    let target_sum = if opts.hash_mode {
        match hash_of(f1, opts.algo) {
            Ok(h) => h,
            Err(_) => return FindResult::NotFound,
        }
    } else {
        String::new()
    };

    for entry in WalkDir::new(dir2).into_iter().filter_map(|e| e.ok()) {
        if max_find_time > 0 && start.elapsed() >= Duration::from_secs(max_find_time) {
            return FindResult::TimedOut;
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            return FindResult::NotFound;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let cand = entry.path();
        if opts.hash_mode {
            if let Ok(h) = cache.hash_of_cached(cand, opts.algo) {
                if h == target_sum {
                    return FindResult::Found(cand.to_path_buf());
                }
            }
        } else if files_equal_bytes(f1, cand).unwrap_or(false) {
            return FindResult::Found(cand.to_path_buf());
        }
    }
    FindResult::NotFound
}

//////////////////////////////
// Per-file check result     //
//////////////////////////////

enum FindResult {
    Found(PathBuf),
    NotFound,
    TimedOut,
}

enum CheckResult {
    Ok,
    Renamed(PathBuf),
    RenamedNotFound, // search was performed but nothing matched
    RenamedTimedOut, // search aborted, --max-find-time exceeded
    Missing,         // --find-renamed not enabled
    Differ,
}

#[allow(clippy::too_many_arguments)]
fn check_one(
    file1: &Path,
    file2: &Path,
    opts: &CompareOpts,
    find_renamed_opt: bool,
    dir2: &Path,
    max_find_time: u64,
    cache: &HashCache,
    status: &Arc<Mutex<Status>>,
    rel_str: &str,
    live_ui: bool,
) -> CheckResult {
    let outer = if live_ui {
        Some(start_spinner(
            status.clone(),
            format!("Checking {rel_str}"),
            WHITE,
            false,
        ))
    } else {
        None
    };

    if !file2.is_file() {
        if find_renamed_opt {
            if let Some(h) = outer {
                stop_spinner(h);
            }
            let finding_handle = if live_ui {
                Some(start_spinner(
                    status.clone(),
                    format!("Find {rel_str}"),
                    CYAN,
                    true,
                ))
            } else {
                None
            };
            let result = find_renamed(dir2, file1, opts, max_find_time, cache);
            if let Some(h) = finding_handle {
                stop_spinner(h);
            }
            return match result {
                FindResult::Found(p) => CheckResult::Renamed(p),
                FindResult::NotFound => CheckResult::RenamedNotFound,
                FindResult::TimedOut => CheckResult::RenamedTimedOut,
            };
        }
        if let Some(h) = outer {
            stop_spinner(h);
        }
        return CheckResult::Missing;
    }
    let eq = compare_files(file1, file2, opts);
    if let Some(h) = outer {
        stop_spinner(h);
    }
    if eq {
        CheckResult::Ok
    } else {
        CheckResult::Differ
    }
}

//////////////////////////////
// Ctrl+C / Ctrl+Z handling  //
//////////////////////////////

fn print_interrupt_summary(reason: &str, code: i32) {
    let total = TOTAL.load(Ordering::SeqCst);
    let index = INDEX.load(Ordering::SeqCst);
    let unchecked = total.saturating_sub(index);

    cleanup_cache_file();

    print!("\r\x1b[J");
    print!("{YELLOW}*{RESET} Verification interrupted [{unchecked}/{total} unchecked]\n");
    print!("\n{RED}*{RESET} {reason}\n");
    print!(
        "{BLUE}*{RESET} OK: {}  Errors: {}  Unchecked: {}\n",
        OK_COUNT.load(Ordering::SeqCst),
        ERR_COUNT.load(Ordering::SeqCst),
        unchecked
    );
    let _ = io::stdout().flush();
    exit(code);
}

fn install_interrupt_handler() {
    ctrlc::set_handler(move || {
        if INTERRUPTED.load(Ordering::SeqCst) {
            print!("\n{RED}*{RESET} Force exit.\n");
            let _ = io::stdout().flush();
            cleanup_cache_file();
            exit(130);
        }
        INTERRUPTED.store(true, Ordering::SeqCst);
        print_interrupt_summary("Interrupted by user.", 130);
    })
    .expect("Error setting Ctrl-C handler");
}

// Ctrl+Z (SIGTSTP) support: letting the default handler suspend the process
// mid-write (while a spinner/worker thread is touching the terminal)
// garbles the screen once the shell resumes the job with `fg`/`bg` — this
// is the "задержки/зависания на ctrl+z" the original script suffered from.
// We intercept SIGTSTP, tell background threads to stop writing, flush and
// print a clean status line, *then* actually stop ourselves; on SIGCONT we
// announce the resume so the next redraw starts from a known-good state.
#[cfg(unix)]
fn install_suspend_handler() {
    use signal_hook::consts::{SIGCONT, SIGTSTP};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGTSTP, SIGCONT]) {
        Ok(s) => s,
        Err(_) => return, // best-effort; not fatal if unsupported
    };

    thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGTSTP => {
                    PAUSED.store(true, Ordering::SeqCst);
                    print!("\r\x1b[J{YELLOW}*{RESET} Paused (Ctrl+Z). Resume with `fg`.\n");
                    let _ = io::stdout().flush();

                    // Give background threads a moment to notice PAUSED and
                    // stop writing before we actually suspend, so we don't
                    // suspend mid-write and tear the terminal state.
                    thread::sleep(Duration::from_millis(30));

                    unsafe {
                        raise_sigstop();
                    }
                    // Execution resumes here once `fg`/`bg`/SIGCONT arrives.
                }
                SIGCONT => {
                    if PAUSED.swap(false, Ordering::SeqCst) {
                        print!("{GREEN}*{RESET} Resumed.\n");
                        let _ = io::stdout().flush();
                    }
                }
                _ => {}
            }
        }
    });
}

#[cfg(not(unix))]
fn install_suspend_handler() {
    // Ctrl+Z / SIGTSTP is a Unix concept; nothing to do elsewhere.
}

#[cfg(unix)]
unsafe fn raise_sigstop() {
    extern "C" {
        fn raise(sig: i32) -> i32;
    }
    const SIGSTOP: i32 = 19;
    raise(SIGSTOP);
}

//////////////////////////////
// File collection           //
//////////////////////////////

fn collect_files(dir1: &Path, follow_symlinks: bool, exclude: &[String]) -> Vec<PathBuf> {
    WalkDir::new(dir1)
        .follow_links(follow_symlinks)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            if exclude.is_empty() {
                return true;
            }
            let rel = p.strip_prefix(dir1).unwrap_or(p);
            let rel_s = rel.to_string_lossy();
            !exclude.iter().any(|pat| rel_s.contains(pat.as_str()))
        })
        .collect()
}

//////////////////////////////
// Reporting                //
//////////////////////////////

fn display_name(dir1: &Path, file1: &Path, verbose: bool) -> String {
    if verbose {
        file1.display().to_string()
    } else {
        file1
            .strip_prefix(dir1)
            .unwrap_or(file1)
            .display()
            .to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    status: &Arc<Mutex<Status>>,
    result: &CheckResult,
    rel_str: &str,
    file1: &Path,
    file2: &Path,
    progress: bool,
    quiet: bool,
) {
    match result {
        CheckResult::Ok => {
            OK_COUNT.fetch_add(1, Ordering::SeqCst);
            if !quiet && !progress {
                let mut st = status.lock().unwrap();
                st.status_line("*", GREEN, &format!("Check {rel_str}"), "ok", GREEN);
                st.finish_line();
            }
        }
        CheckResult::Renamed(found_path) => {
            OK_COUNT.fetch_add(1, Ordering::SeqCst);
            if !quiet {
                let mut st = status.lock().unwrap();
                if !progress {
                    st.status_line("*", GREEN, &format!("Find {rel_str}"), "found", GREEN);
                    st.finish_line();
                }
                drop(st);
                println!("{GREEN}*{RESET} Looks like the file was renamed/moved:");
                println!("    Found as: {}\n", found_path.display());
            }
        }
        CheckResult::RenamedNotFound => {
            ERR_COUNT.fetch_add(1, Ordering::SeqCst);
            let mut st = status.lock().unwrap();
            if !progress {
                st.status_line("*", CYAN, &format!("Find {rel_str}"), "not found", RED);
                st.finish_line();
            }
            drop(st);
            println!(
                "{RED}*{RESET} File not found in destination, and no renamed match was found:"
            );
            println!("    {}\n", file2.display());
        }
        CheckResult::RenamedTimedOut => {
            ERR_COUNT.fetch_add(1, Ordering::SeqCst);
            let mut st = status.lock().unwrap();
            if !progress {
                st.status_line("*", CYAN, &format!("Find {rel_str}"), "outTime", YELLOW);
                st.finish_line();
            }
            drop(st);
            println!("{YELLOW}*{RESET} Rename search aborted: --max-find-time exceeded before a match was found.");
            println!("    {}\n", file2.display());
        }
        CheckResult::Missing => {
            ERR_COUNT.fetch_add(1, Ordering::SeqCst);
            let mut st = status.lock().unwrap();
            if !progress {
                st.status_line("*", RED, &format!("Check {rel_str}"), "!!", RED);
                st.finish_line();
            }
            drop(st);
            println!("{RED}*{RESET} File not found in destination:");
            println!("    {}\n", file2.display());
        }
        CheckResult::Differ => {
            ERR_COUNT.fetch_add(1, Ordering::SeqCst);
            let mut st = status.lock().unwrap();
            if !progress {
                st.status_line("*", RED, &format!("Check {rel_str}"), "!!", RED);
                st.finish_line();
            }
            drop(st);
            println!("{RED}*{RESET} File contents differ.");
            println!("{RED}*{RESET} The file may be corrupted or modified.");
            println!("    Source      : {}", file1.display());
            println!("    Destination : {}\n", file2.display());
        }
    }
}

//////////////////////////////
// Sequential run (jobs<=1)  //
//////////////////////////////

#[allow(clippy::too_many_arguments)]
fn run_sequential(
    dir1: &Path,
    dir2: &Path,
    files: &[PathBuf],
    opts: &CompareOpts,
    find_renamed_opt: bool,
    max_find_time: u64,
    cache: &HashCache,
    status: &Arc<Mutex<Status>>,
    progress: bool,
    quiet: bool,
    verbose: bool,
) {
    let total = files.len();
    let live_ui = !progress && !quiet;
    for (index, file1) in files.iter().enumerate() {
        INDEX.store(index, Ordering::SeqCst);

        let rel = file1.strip_prefix(dir1).unwrap_or(file1).to_path_buf();
        let file2 = dir2.join(&rel);
        let rel_str = display_name(dir1, file1, verbose);

        let result = check_one(
            file1,
            &file2,
            opts,
            find_renamed_opt,
            dir2,
            max_find_time,
            cache,
            status,
            &rel_str,
            live_ui,
        );

        if progress {
            let mut st = status.lock().unwrap();
            st.progress_line(index + 1, total, ERR_COUNT.load(Ordering::SeqCst), &rel_str);
        }

        report(status, &result, &rel_str, file1, &file2, progress, quiet);
    }
    if progress {
        println!();
    }
    INDEX.store(total, Ordering::SeqCst);
}

//////////////////////////////
// Parallel run (jobs>1)     //
//////////////////////////////

#[allow(clippy::too_many_arguments)]
fn run_parallel(
    dir1: &Path,
    dir2: &Path,
    files: &[PathBuf],
    opts: &CompareOpts,
    find_renamed_opt: bool,
    max_find_time: u64,
    cache: &Arc<HashCache>,
    status: &Arc<Mutex<Status>>,
    progress: bool,
    quiet: bool,
    verbose: bool,
    jobs: usize,
) {
    let total = files.len();
    let next_index = Arc::new(AtomicUsize::new(0));
    // Each slot is filled exactly once by whichever worker claims that index.
    let slots: Arc<Vec<Mutex<Option<CheckResult>>>> =
        Arc::new((0..total).map(|_| Mutex::new(None)).collect());

    let dir1_owned = dir1.to_path_buf();
    let dir2_owned = dir2.to_path_buf();
    let files_owned = files.to_vec();

    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let next_index = next_index.clone();
        let slots = slots.clone();
        let files = files_owned.clone();
        let dir1c = dir1_owned.clone();
        let dir2c = dir2_owned.clone();
        let opts = opts.clone();
        let cache = cache.clone();
        let status = status.clone();
        workers.push(thread::spawn(move || loop {
            if INTERRUPTED.load(Ordering::SeqCst) {
                return;
            }
            while PAUSED.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
            let i = next_index.fetch_add(1, Ordering::SeqCst);
            if i >= files.len() {
                return;
            }
            let file1 = &files[i];
            let rel = file1.strip_prefix(&dir1c).unwrap_or(file1).to_path_buf();
            let file2 = dir2c.join(&rel);
            let rel_str = display_name(&dir1c, file1, verbose);
            // live_ui=false: with several workers running concurrently a
            // per-file spinner would tear the shared status line, so the
            // live "Checking"/"finding" spinner is sequential-mode only.
            let result = check_one(
                file1,
                &file2,
                &opts,
                find_renamed_opt,
                &dir2c,
                max_find_time,
                &cache,
                &status,
                &rel_str,
                false,
            );
            *slots[i].lock().unwrap() = Some(result);
        }));
    }

    // Main thread: drain results strictly in index order so output stays
    // deterministic even though the workers finish out of order.
    for i in 0..total {
        loop {
            if INTERRUPTED.load(Ordering::SeqCst) {
                for w in workers {
                    let _ = w.join();
                }
                return;
            }
            let mut guard = slots[i].lock().unwrap();
            if let Some(result) = guard.take() {
                drop(guard);

                INDEX.store(i, Ordering::SeqCst);
                let file1 = &files[i];
                let rel = file1.strip_prefix(dir1).unwrap_or(file1).to_path_buf();
                let file2 = dir2.join(&rel);
                let rel_str = display_name(dir1, file1, verbose);

                if progress {
                    let mut st = status.lock().unwrap();
                    st.progress_line(i + 1, total, ERR_COUNT.load(Ordering::SeqCst), &rel_str);
                }
                report(status, &result, &rel_str, file1, &file2, progress, quiet);
                break;
            }
            drop(guard);
            thread::sleep(Duration::from_millis(2));
        }
    }

    for w in workers {
        let _ = w.join();
    }
    if progress {
        println!();
    }
    INDEX.store(total, Ordering::SeqCst);
}

//////////////////////////////
// Main                     //
//////////////////////////////

fn main() {
    let prog = std::env::args()
        .next()
        .unwrap_or_else(|| "compare_dirs".to_string());
    let raw = RawArgs::parse();

    if raw.help {
        usage(&prog);
        exit(0);
    }
    if raw.version {
        println!("compare_dirs {}", env!("CARGO_PKG_VERSION"));
        exit(0);
    }

    println!("{GREEN}*{RESET} Please wait.");

    let run_start = Instant::now();

    let hash_mode = raw.hash;
    let algo_name = raw.algo;

    let min_size = match raw.min_size {
        Some(s) => match parse_size(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                exit(1);
            }
        },
        None => 0,
    };
    let max_size = match raw.max_size {
        Some(s) => match parse_size(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                exit(1);
            }
        },
        None => 0,
    };
    if max_size > 0 && min_size > max_size {
        eprintln!("Error: --min-size cannot be greater than --max-size");
        exit(1);
    }
    let max_find_time = match raw.max_find_time {
        Some(s) => match parse_time(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                exit(1);
            }
        },
        None => 0,
    };
    let find_renamed_opt = raw.find_renamed;

    let positional = raw.positional;
    if positional.len() != 2 {
        usage(&prog);
        exit(1);
    }
    let dir1 = PathBuf::from(positional[0].trim_end_matches('/'));
    let dir2 = PathBuf::from(positional[1].trim_end_matches('/'));

    if !dir1.is_dir() {
        eprintln!("Directory not found: {}", dir1.display());
        exit(1);
    }
    if !dir2.is_dir() {
        eprintln!("Directory not found: {}", dir2.display());
        exit(1);
    }

    if hash_mode && !ALLOWED_ALGOS.contains(&algo_name.as_str()) {
        eprintln!("Unsupported hash algorithm: {algo_name}");
        eprintln!("Available: {}", ALLOWED_ALGOS.join(" "));
        exit(1);
    }
    let algo = if hash_mode {
        HashAlgo::from_name(&algo_name).unwrap()
    } else {
        HashAlgo::Sha256
    };

    let jobs = match raw.jobs {
        None => 1,
        Some(0) => num_cpus::get().max(1),
        Some(n) => n.max(1),
    };

    install_interrupt_handler();
    install_suspend_handler();

    let opts = CompareOpts {
        hash_mode,
        algo,
        min_size,
        max_size,
    };

    // Collect files (mirrors `find "$DIR1" -type f`)
    let files = collect_files(&dir1, raw.follow_symlinks, &raw.exclude);

    if raw.dry_run {
        println!(
            "{BLUE}*{RESET} Dry run: {} file(s) would be checked.",
            files.len()
        );
        for f in &files {
            let rel = f.strip_prefix(&dir1).unwrap_or(f);
            println!("    {}", rel.display());
        }
        exit(0);
    }

    TOTAL.store(files.len(), Ordering::SeqCst);

    let status = Arc::new(Mutex::new(Status::new()));

    // Hash cache: only useful with --hash + --find-renamed, since that's
    // the combination that repeatedly re-hashes DIR2. Enabled by default
    // in that case, disabled otherwise (or if --no-hash-cache is given).
    let cache_enabled = hash_mode && find_renamed_opt && !raw.no_hash_cache;
    let cache_path: Option<PathBuf> = if cache_enabled {
        let p = match &raw.cache_file {
            Some(custom) => PathBuf::from(custom),
            None => {
                std::env::temp_dir().join(format!("compareDirs2-hashes-{}.tsv", std::process::id()))
            }
        };
        register_cache_path(p.clone());
        KEEP_CACHE_FILE.store(raw.keep_cache_file, Ordering::SeqCst);
        Some(p)
    } else {
        None
    };
    let cache = Arc::new(HashCache::new(cache_enabled, cache_path.as_deref()));

    if jobs <= 1 {
        run_sequential(
            &dir1,
            &dir2,
            &files,
            &opts,
            find_renamed_opt,
            max_find_time,
            &cache,
            &status,
            raw.progress,
            raw.quiet,
            raw.verbose,
        );
    } else {
        run_parallel(
            &dir1,
            &dir2,
            &files,
            &opts,
            find_renamed_opt,
            max_find_time,
            &cache,
            &status,
            raw.progress,
            raw.quiet,
            raw.verbose,
            jobs,
        );
    }

    cleanup_cache_file();

    println!("\n{GREEN}*{RESET} Verification finished.");
    println!(
        "{BLUE}*{RESET} OK: {}  Errors: {}",
        OK_COUNT.load(Ordering::SeqCst),
        ERR_COUNT.load(Ordering::SeqCst)
    );
    if raw.stats {
        let elapsed = run_start.elapsed();
        let secs = elapsed.as_secs_f64().max(0.000_001);
        let total = TOTAL.load(Ordering::SeqCst);
        println!(
            "{BLUE}*{RESET} Elapsed: {:.2}s  ({:.1} files/s)",
            secs,
            total as f64 / secs
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_units() {
        assert_eq!(parse_size("1tb").unwrap(), 1_000_000_000_000);
        assert_eq!(parse_size("1pb").unwrap(), 1_000_000_000_000_000);
        assert_eq!(parse_size("1kib").unwrap(), 1024);
        assert_eq!(parse_size("500b").unwrap(), 500);
        assert_eq!(parse_size("2mb").unwrap(), 2_000_000);
        assert!(parse_size("bad").is_err());
    }

    #[test]
    fn time_units() {
        assert_eq!(parse_time("5s").unwrap(), 5);
        assert_eq!(parse_time("2m").unwrap(), 120);
        assert_eq!(parse_time("1h").unwrap(), 3600);
        assert_eq!(parse_time("1w").unwrap(), 604_800);
    }
}
