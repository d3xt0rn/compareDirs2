// compare_dirs.rs
//
// Rust port of compareDirs2.sh
//
// Compare all files in DIR1 with DIR2 recursively.
// Styled after OpenRC.

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
    about = "Compare all files in DIR1 with DIR2 recursively.",
    disable_help_flag = true
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

    #[arg(short = 'h', long = "help", action = clap::ArgAction::SetTrue)]
    help: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    positional: Vec<String>,
}

fn usage(prog: &str) {
    println!(
        r#"Usage: {prog} [OPTIONS] DIR1 DIR2

Options:
  -x, --hash           Compare files by hash instead of byte-for-byte cmp
  -a, --algo ALGO      Hash algorithm: md5sum|sha1sum|sha256sum|sha512sum|b2sum
                       (default: sha256sum)
      --min-size SIZE  Only hash-compare files >= SIZE (e.g., 500B, 2KB, 10MiB, 1GB)
                       (smaller files use cmp)
      --max-size SIZE  Only hash-compare files <= SIZE (e.g., 100MB, 2GiB)
                       (larger files use cmp)
      --max-find-time T Max time allowed for find search per file (e.g., 5s, 2m, 1h)
  -r, --find-renamed   If a file is missing in DIR2 — search DIR2 for a
                       file with identical content/hash (rename detection)
  -h, --help           Show this help
"#
    );
}

//////////////////////////////
// Parsers (Size & Time)    //
//////////////////////////////

// Converts human-readable size to bytes (mirrors the bash parse_size, bugs included)
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

        let factor: u64 = match unit {
            "b" => 1,
            "k" | "kb" => 1_000,
            "m" | "mb" => 1_000_000,
            "g" | "gb" => 1_000_000_000,
            "t" | "tb" => 100_000_000_000, // matches original script's value
            "p" | "pb" => 100_000_000_000_000, // matches original script's value
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
        Ok(num * factor)
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
        Ok(num * factor)
    } else {
        Err(format!("Error: Invalid time format '{val}'"))
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
    Blake2b,
}

impl HashAlgo {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "md5sum" => Some(HashAlgo::Md5),
            "sha1sum" => Some(HashAlgo::Sha1),
            "sha256sum" => Some(HashAlgo::Sha256),
            "sha512sum" => Some(HashAlgo::Sha512),
            "b2sum" => Some(HashAlgo::Blake2b),
            _ => None,
        }
    }
}

const ALLOWED_ALGOS: [&str; 5] = ["md5sum", "sha1sum", "sha256sum", "sha512sum", "b2sum"];

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
        HashAlgo::Blake2b => digest_loop!(blake2::Blake2b512::new()),
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
                "{pcolor}{prefix}{RESET} {msg}{pad}{blue}[{RESET} {scolor}{status}{RESET} {blue}]{RESET}",
                pcolor = pcolor,
                prefix = prefix,
                RESET = RESET,
                msg = msg,
                pad = " ".repeat(pad),
                blue = BLUE,
                scolor = scolor,
                status = status,
            );
            self.last_lines = 1;
        } else if term_w >= msglen + 2 {
            let _ = writeln!(
                out,
                "{pcolor}{prefix}{RESET} {msg}",
                pcolor = pcolor,
                prefix = prefix,
                RESET = RESET,
                msg = msg
            );
            let _ = write!(
                out,
                "    {blue}[{RESET} {scolor}{status}{RESET} {blue}]{RESET}",
                blue = BLUE,
                RESET = RESET,
                scolor = scolor,
                status = status,
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
                "{pcolor}{prefix}{RESET} {msg}{pad}{blue}[{RESET} {scolor}{status}{RESET} {blue}]{RESET}",
                pcolor = pcolor,
                prefix = prefix,
                RESET = RESET,
                msg = msg,
                pad = " ".repeat(pad),
                blue = BLUE,
                scolor = scolor,
                status = status,
            );
            self.last_lines = 1;
        }
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
// Ctrl+C handler)          //
//////////////////////////////

static OK_COUNT: AtomicUsize = AtomicUsize::new(0);
static ERR_COUNT: AtomicUsize = AtomicUsize::new(0);
static INDEX: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

//////////////////////////////
// Compare logic             //
//////////////////////////////

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

fn find_renamed(dir2: &Path, f1: &Path, opts: &CompareOpts, max_find_time: u64) -> Option<PathBuf> {
    let start = Instant::now();
    let target_sum = if opts.hash_mode {
        hash_of(f1, opts.algo).unwrap_or_default()
    } else {
        String::new()
    };

    for entry in WalkDir::new(dir2).into_iter().filter_map(|e| e.ok()) {
        if max_find_time > 0 && start.elapsed() >= Duration::from_secs(max_find_time) {
            return None;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let cand = entry.path();
        if opts.hash_mode {
            if let Ok(h) = hash_of(cand, opts.algo) {
                if h == target_sum {
                    return Some(cand.to_path_buf());
                }
            }
        } else if files_equal_bytes(f1, cand).unwrap_or(false) {
            return Some(cand.to_path_buf());
        }
    }
    None
}

//////////////////////////////
// Ctrl+C handling           //
//////////////////////////////

fn install_interrupt_handler() {
    ctrlc::set_handler(move || {
        if INTERRUPTED.load(Ordering::SeqCst) {
            print!("\n{RED}*{RESET} Force exit.\n");
            let _ = io::stdout().flush();
            exit(130);
        }
        INTERRUPTED.store(true, Ordering::SeqCst);

        let total = TOTAL.load(Ordering::SeqCst);
        let index = INDEX.load(Ordering::SeqCst);
        let unchecked = total.saturating_sub(index);

        // Clear whatever partial status line is on screen and print the summary.
        print!("\r\x1b[J");
        print!("{YELLOW}*{RESET} Verification interrupted [{unchecked}/{total} unchecked]\n");
        print!("\n{RED}*{RESET} Interrupted by user.\n");
        print!(
            "{BLUE}*{RESET} OK: {}  Errors: {}  Unchecked: {}\n",
            OK_COUNT.load(Ordering::SeqCst),
            ERR_COUNT.load(Ordering::SeqCst),
            unchecked
        );
        let _ = io::stdout().flush();
        exit(130);
    })
    .expect("Error setting Ctrl-C handler");
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

    install_interrupt_handler();

    let opts = CompareOpts {
        hash_mode,
        algo,
        min_size,
        max_size,
    };

    // Collect files (mirrors `find "$DIR1" -type f`)
    let files: Vec<PathBuf> = WalkDir::new(&dir1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();

    TOTAL.store(files.len(), Ordering::SeqCst);

    let status = Arc::new(Mutex::new(Status::new()));

    for (index, file1) in files.iter().enumerate() {
        INDEX.store(index, Ordering::SeqCst);

        let rel = file1.strip_prefix(&dir1).unwrap_or(file1).to_path_buf();
        let file2 = dir2.join(&rel);
        let rel_str = rel.display().to_string();

        //////////////////////////////////////
        // Missing file
        //////////////////////////////////////
        if !file2.is_file() {
            if find_renamed_opt {
                let handle = start_spinner(status.clone(), format!("Find {rel_str}"), CYAN, true);
                let found = find_renamed(&dir2, file1, &opts, max_find_time);
                stop_spinner(handle);

                if let Some(found_path) = found {
                    {
                        let mut st = status.lock().unwrap();
                        st.status_line("*", GREEN, &format!("Find {rel_str}"), "found", GREEN);
                        st.finish_line();
                    }
                    println!("{GREEN}*{RESET} Looks like the file was renamed/moved:");
                    println!("    Found as: {}\n", found_path.display());
                    OK_COUNT.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
            }
            {
                let mut st = status.lock().unwrap();
                st.status_line("*", RED, &format!("Check {rel_str}"), "!!", RED);
                st.finish_line();
            }
            println!("{RED}*{RESET} File not found in destination:");
            println!("    {}\n", file2.display());
            ERR_COUNT.fetch_add(1, Ordering::SeqCst);
            continue;
        }

        //////////////////////////////////////
        // Spinner + compare
        //////////////////////////////////////
        let handle = start_spinner(status.clone(), format!("Checking {rel_str}"), WHITE, false);
        let equal = compare_files(file1, &file2, &opts);
        stop_spinner(handle);

        if equal {
            let mut st = status.lock().unwrap();
            st.status_line("*", GREEN, &format!("Check {rel_str}"), "ok", GREEN);
            st.finish_line();
            drop(st);
            OK_COUNT.fetch_add(1, Ordering::SeqCst);
        } else {
            {
                let mut st = status.lock().unwrap();
                st.status_line("*", RED, &format!("Check {rel_str}"), "!!", RED);
                st.finish_line();
            }
            println!("{RED}*{RESET} File contents differ.");
            println!("{RED}*{RESET} The file may be corrupted or modified.");
            println!("    Source      : {}", file1.display());
            println!("    Destination : {}\n", file2.display());
            ERR_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    println!("\n{GREEN}*{RESET} Verification finished.");
    println!(
        "{BLUE}*{RESET} OK: {}  Errors: {}",
        OK_COUNT.load(Ordering::SeqCst),
        ERR_COUNT.load(Ordering::SeqCst)
    );
}
