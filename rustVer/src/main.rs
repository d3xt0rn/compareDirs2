use clap::Parser;
use crossterm::{
    cursor,
    style::{Color, Stylize},
    terminal, ExecutableCommand,
};
use digest::Digest;
use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

const SPINNER: [&str; 4] = ["/", "-", "\\", "|"];

#[derive(Parser, Debug)]
#[command(
    name = "compare_dirs",
    help_template = "\
Usage: {bin} [OPTIONS] <DIR1> <DIR2>

Options:
  -x, --hash           Compare files by hash instead of byte-for-byte cmp
  -a, --algo <ALGO>    Hash algorithm: md5sum|sha1sum|sha256sum|sha512sum|b2sum [default: sha256sum]
      --min-size <SZ>  Only hash-compare files >= SIZE (e.g., 500B, 2KB, 10MiB, 1GB) (smaller files use cmp)
      --max-size <SZ>  Only hash-compare files <= SIZE (e.g., 100MB, 2GiB) (larger files use cmp)
      --max-find-time <T> Max time allowed for find search per file (e.g., 5s, 2m, 1h)
  -r, --find-renamed   If a file is missing in DIR2 — search DIR2 for a file with identical content/hash (rename detection)
  -h, --help           Show this help
"
)]
struct Args {
    #[arg(short = 'x', long)]
    hash: bool,

    #[arg(short = 'a', long, default_value = "sha256sum")]
    algo: String,

    #[arg(long, value_parser = parse_size)]
    min_size: Option<u64>,

    #[arg(long, value_parser = parse_size)]
    max_size: Option<u64>,

    #[arg(long, value_parser = parse_time)]
    max_find_time: Option<Duration>,

    #[arg(short = 'r', long)]
    find_renamed: bool,

    dir1: PathBuf,
    dir2: PathBuf,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<u64>().map_err(|e| e.to_string());
    }

    let digit_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num_str, unit) = s.split_at(digit_end);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("Invalid number: {}", num_str))?;

    let factor: u64 = match unit {
        "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "t" | "tb" => 100_000_000_000,
        "p" | "pb" => 100_000_000_000_000,
        "ki" | "kib" => 1_024,
        "mi" | "mib" => 1_048_576,
        "gi" | "gib" => 1_073_741_824,
        "ti" | "tib" => 1_099_511_627_776,
        "pi" | "pib" => 1_125_899_906_842_624,
        _ => return Err(format!("Unknown size suffix '{}'", unit)),
    };

    Ok(num * factor)
}

fn parse_time(s: &str) -> Result<Duration, String> {
    let s = s.trim().to_lowercase();
    if s.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Duration::from_secs(
            s.parse::<u64>().map_err(|e| e.to_string())?,
        ));
    }

    let digit_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num_str, unit) = s.split_at(digit_end);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("Invalid number: {}", num_str))?;

    let secs = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        "w" => num * 604800,
        _ => return Err(format!("Unknown time suffix '{}'", unit)),
    };

    Ok(Duration::from_secs(secs))
}

fn get_terminal_width() -> u16 {
    terminal::size().map(|(w, _)| w).unwrap_or(80)
}

fn compute_hash(path: &Path, algo: &str) -> io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buffer = [0; 8192];

    macro_rules! hash_file {
        ($hasher:ty) => {{
            let mut hasher = <$hasher>::new();
            loop {
                let bytes_read = reader.read(&mut buffer)?;
                if bytes_read == 0 {
                    break;
                }
                hasher.update(&buffer[..bytes_read]);
            }
            format!("{:x}", hasher.finalize())
        }};
    }

    let hex = match algo {
        "md5sum" => hash_file!(md5::Md5),
        "sha1sum" => hash_file!(sha1::Sha1),
        "sha256sum" => hash_file!(sha2::Sha256),
        "sha512sum" => hash_file!(sha2::Sha512),
        "b2sum" => hash_file!(blake2::Blake2b512),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unsupported algo",
            ))
        }
    };
    Ok(hex)
}

fn compare_files_byte_by_byte(p1: &Path, p2: &Path) -> io::Result<bool> {
    let mut f1 = File::open(p1)?;
    let mut f2 = File::open(p2)?;
    let mut buf1 = [0; 8192];
    let mut buf2 = [0; 8192];

    loop {
        let n1 = f1.read(&mut buf1)?;
        let n2 = f2.read(&mut buf2)?;
        if n1 != n2 || buf1[..n1] != buf2[..n2] {
            return Ok(false);
        }
        if n1 == 0 {
            return Ok(true);
        }
    }
}

fn should_hash(path: &Path, args: &Args) -> bool {
    if !args.hash {
        return false;
    }
    if let Ok(meta) = fs::metadata(path) {
        let size = meta.len();
        if let Some(min) = args.min_size {
            if size < min {
                return false;
            }
        }
        if let Some(max) = args.max_size {
            if size > max {
                return false;
            }
        }
    }
    true
}

fn check_match(p1: &Path, p2: &Path, args: &Args, use_hash: bool) -> bool {
    if use_hash {
        if let (Ok(h1), Ok(h2)) = (compute_hash(p1, &args.algo), compute_hash(p2, &args.algo)) {
            return h1 == h2;
        }
        return false;
    }
    compare_files_byte_by_byte(p1, p2).unwrap_or(false)
}

fn clear_status(stdout: &mut io::Stdout, last_lines: usize) -> io::Result<()> {
    if last_lines > 1 {
        stdout.execute(cursor::MoveUp((last_lines - 1) as u16))?;
    }
    print!("\r");
    stdout.execute(terminal::Clear(terminal::ClearType::FromCursorDown))?;
    Ok(())
}

fn status_line(
    stdout: &mut io::Stdout,
    prefix: &str,
    prefix_color: Color,
    msg: &str,
    status: &str,
    status_color: Color,
    last_lines: &mut usize,
) -> io::Result<()> {
    clear_status(stdout, *last_lines)?;
    let term_width = get_terminal_width() as usize;

    let status_len = status.chars().count();
    let fixed = status_len + 6;
    let msg_len = msg.chars().count();

    if term_width >= msg_len + fixed + 2 {
        let pad = term_width.saturating_sub(msg_len).saturating_sub(fixed);
        print!(
            "{} {}{:>pad$}{}[ {}{} {}]",
            prefix.with(prefix_color).bold(),
            msg,
            "",
            "[".with(Color::Blue).bold(),
            status.with(status_color).bold(),
            "".with(Color::Reset),
            "]".with(Color::Blue).bold(),
            pad = pad
        );
        *last_lines = 1;
    } else if term_width >= msg_len + 2 {
        println!("{} {}", prefix.with(prefix_color).bold(), msg);
        print!(
            "    {}[ {}{} {}]",
            "[".with(Color::Blue).bold(),
            status.with(status_color).bold(),
            "".with(Color::Reset),
            "]".with(Color::Blue).bold()
        );
        *last_lines = 2;
    } else {
        let mut max_msg = term_width.saturating_sub(fixed).saturating_sub(2);
        if max_msg < 5 {
            max_msg = 5;
        }
        let truncated_msg: String = if msg_len > max_msg {
            let start_idx = msg
                .char_indices()
                .map(|(i, _)| i)
                .nth(msg_len - (max_msg - 3))
                .unwrap_or(0);
            format!("...{}", &msg[start_idx..])
        } else {
            msg.to_string()
        };
        let t_len = truncated_msg.chars().count();
        let pad = term_width.saturating_sub(t_len).saturating_sub(fixed);
        print!(
            "{} {}{:>pad$}{}[ {}{} {}]",
            prefix.with(prefix_color).bold(),
            truncated_msg,
            "",
            "[".with(Color::Blue).bold(),
            status.with(status_color).bold(),
            "".with(Color::Reset),
            "]".with(Color::Blue).bold(),
            pad = pad
        );
        *last_lines = 1;
    }
    io::stdout().flush()?;
    Ok(())
}

struct SpinnerHandle {
    stop_flag: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SpinnerHandle {
    fn start(msg: String, color: Color, is_finding: bool) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = stop_flag.clone();

        let handle = thread::spawn(move || {
            let mut stdout = io::stdout();
            let mut last_lines = 1;
            let mut idx = 0;
            while !flag_clone.load(Ordering::Relaxed) {
                let frame = SPINNER[idx % SPINNER.len()];
                let status_str = if is_finding {
                    format!("finding {}", frame)
                } else {
                    frame.to_string()
                };

                let _ = status_line(
                    &mut stdout,
                    " ",
                    Color::Reset,
                    &msg,
                    &status_str,
                    color,
                    &mut last_lines,
                );
                thread::sleep(Duration::from_millis(80));
                idx += 1;
            }
            let _ = clear_status(&mut stdout, last_lines);
        });

        Self {
            stop_flag,
            handle: Some(handle),
        }
    }

    fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            self.stop_flag.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let allowed_algos = ["md5sum", "sha1sum", "sha256sum", "sha512sum", "b2sum"];
    if args.hash && !allowed_algos.contains(&args.algo.as_str()) {
        eprintln!("Unsupported hash algorithm: {}", args.algo);
        eprintln!("Available: {:?}", allowed_algos);
        std::process::exit(1);
    }

    if !args.dir1.is_dir() {
        eprintln!("Directory not found: {:?}", args.dir1);
        std::process::exit(1);
    }
    if !args.dir2.is_dir() {
        eprintln!("Directory not found: {:?}", args.dir2);
        std::process::exit(1);
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Setup graceful Ctrl+C interruption handler
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl+C handler");

    let mut files = Vec::new();
    for entry in WalkDir::new(&args.dir1).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }

    let total = files.len();
    let mut ok_count = 0;
    let mut err_count = 0;
    let mut last_lines = 1;
    let mut stdout = io::stdout();

    for (index, file1) in files.iter().enumerate() {
        if !running.load(Ordering::SeqCst) {
            let unchecked = total - index;
            status_line(
                &mut stdout,
                "*",
                Color::Yellow,
                "Verification interrupted",
                &format!("{}/{} unchecked", unchecked, total),
                Color::Yellow,
                &mut last_lines,
            )?;
            println!();
            println!(
                "{}",
                "\n* Force exit or Interrupted by user."
                    .with(Color::Red)
                    .bold()
            );
            println!(
                "{} OK: {}  Errors: {}  Unchecked: {}",
                "*".with(Color::Blue).bold(),
                ok_count,
                err_count,
                unchecked
            );
            std::process::exit(130);
        }

        let rel_path = file1.strip_prefix(&args.dir1).unwrap();
        let rel_str = rel_path.to_string_lossy();
        let file2 = args.dir2.join(rel_path);

        // Missing file processing
        if !file2.is_file() {
            if args.find_renamed {
                let msg = format!("Find {}", rel_str);
                let mut spinner = SpinnerHandle::start(msg, Color::Cyan, true);

                let start_find = Instant::now();
                let mut found_path: Option<PathBuf> = None;
                let use_hash = should_hash(file1, &args);

                for entry in WalkDir::new(&args.dir2).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        if let Some(max_t) = args.max_find_time {
                            if start_find.elapsed() >= max_t {
                                break;
                            }
                        }
                        if check_match(file1, entry.path(), &args, use_hash) {
                            found_path = Some(entry.into_path());
                            break;
                        }
                    }
                }
                spinner.stop();

                if let Some(fp) = found_path {
                    status_line(
                        &mut stdout,
                        "*",
                        Color::Green,
                        &format!("Find {}", rel_str),
                        "found",
                        Color::Green,
                        &mut last_lines,
                    )?;
                    println!();
                    println!(
                        "{}",
                        "* Looks like the file was renamed/moved:"
                            .with(Color::Green)
                            .bold()
                    );
                    println!("    Found as: {}", fp.display());
                    println!();
                    ok_count += 1;
                    continue;
                }
            }

            status_line(
                &mut stdout,
                "*",
                Color::Red,
                &format!("Check {}", rel_str),
                "!!",
                Color::Red,
                &mut last_lines,
            )?;
            println!();
            println!(
                "{}",
                "* File not found in destination:".with(Color::Red).bold()
            );
            println!("    {}", file2.display());
            println!();
            err_count += 1;
            continue;
        }

        // Compare standard matching file processing
        let msg = format!("Checking {}", rel_str);
        let mut spinner = SpinnerHandle::start(msg, Color::White, false);

        let use_hash = should_hash(file1, &args);
        let is_match = check_match(file1, &file2, &args, use_hash);

        spinner.stop();

        if is_match {
            status_line(
                &mut stdout,
                "*",
                Color::Green,
                &format!("Check {}", rel_str),
                "ok",
                Color::Green,
                &mut last_lines,
            )?;
            println!();
            ok_count += 1;
        } else {
            status_line(
                &mut stdout,
                "*",
                Color::Red,
                &format!("Check {}", rel_str),
                "!!",
                Color::Red,
                &mut last_lines,
            )?;
            println!();
            println!("{}", "* File contents differ.".with(Color::Red).bold());
            println!(
                "{}",
                "* The file may be corrupted or modified."
                    .with(Color::Red)
                    .bold()
            );
            println!("    Source      : {}", file1.display());
            println!("    Destination : {}", file2.display());
            println!();
            err_count += 1;
        }
    }

    println!("{}", "\n* Verification finished.".with(Color::Green).bold());
    println!(
        "{} OK: {}  Errors: {}",
        "*".with(Color::Blue).bold(),
        ok_count,
        err_count
    );

    Ok(())
}
