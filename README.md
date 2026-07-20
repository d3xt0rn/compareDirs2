# compareDirs2

A recursive directory comparison tool, styled after **OpenRC**'s
service-status output (`* Checking foo ... [ ok ]`) and, in `--progress`
mode, after **emerge**'s build progress bar.

> [!note]
> This is a full Rust rewrite of the original `compareDirs2.sh` Bash
> script — same look & feel, but multithreaded, with more hash algorithms,
> exclude filters, a progress-bar mode, and proper `Ctrl+C` / `Ctrl+Z`
> handling that doesn't tear up the terminal.

It walks every regular file under `DIR1` and checks whether an identical
file exists at the same relative path in `DIR2` — either byte-for-byte
(`cmp`-style) or by hash — with a live spinner (or a single-line progress
bar), color-coded results, and optional rename/move detection.

> [!important]
> This is a **100% vibe-coded** project. It works, it's been used, but it
> hasn't been formally audited — read the source before trusting it with
> anything you can't afford to lose.

## Features

- **OpenRC-style** status lines with a spinning cursor (`/ - \ |`) while a
  file is being checked, or an **emerge-style** single-line progress bar
  (`-p/--progress`).
- Byte-for-byte comparison by default, or hash-based comparison
  (`-x/--hash`) with a choice of **12** algorithms: `md5sum`, `sha1sum`,
  `sha256sum`, `sha512sum`, `sha3-256sum`, `sha3-512sum`, `b2sum` (BLAKE2b),
  `b3sum` (BLAKE3), `xxh32sum`, `xxh64sum`, `xxh3sum`, `crc32sum`.
- Size thresholds (`--min-size` / `--max-size`) to control *when* hashing
  is used, falling back to byte comparison outside the given range.
- Rename/move detection (`-r/--find-renamed`): if a file is missing at the
  expected path, the whole `DIR2` tree is searched for a file with
  matching content/hash. Search time per file can be capped with
  `--max-find-time`.
- **Parallel checking** (`-j/--jobs N`) — spin up N worker threads
  (`0` = auto-detect CPU count), while output is still printed
  strictly in file order, so results stay deterministic and readable.
- Adaptive layout: status text wraps to a second (indented) line, or the
  filename is truncated from the front, depending on the current terminal
  width — recalculated on every redraw.
- `--exclude SUBSTR` (repeatable) to skip paths containing a given
  substring, and `-L/--follow-symlinks` to follow symlinks while walking.
- `--dry-run` to just list what *would* be checked, without touching
  anything.
- `-q/--quiet` to print only errors and the final summary; `-v/--verbose`
  to always print absolute paths instead of paths relative to `DIR1`.
- Clean `Ctrl+C` handling: prints an "interrupted" summary
  (OK / errors / unchecked) and exits with code `130`.
- Clean `Ctrl+Z` handling: background threads are told to stop touching the
  terminal *before* the process actually suspends via `SIGSTOP`, and a
  `Resumed.` line is printed on `fg`/`SIGCONT` — no garbled terminal state
  on resume.
- A short summary (OK / errors) at the end of every run.

## Installation

Build with Cargo:

```bash
cargo build --release
```

Optionally move the binary somewhere on your `$PATH`:

```bash
doas cp target/release/compareDirs2 /usr/local/bin/compareDirs2
```

## Usage

```
compareDirs2 [OPTIONS] DIR1 DIR2
```

`DIR1` is treated as the source of truth: every regular file found under
it is looked for under `DIR2` at the same relative path.

| Option                  | Description                                                                                                  |
|--------------------------|----------------------------------------------------------------------------------------------------------------|
| `-x`, `--hash`           | Compare files by hash instead of byte-for-byte comparison.                                                   |
| `-a`, `--algo ALGO`      | Hash algorithm to use (default: `sha256sum`). See the [Hash algorithms](#hash-algorithms) table below.        |
| `--min-size SIZE`        | Only hash-compare files `>= SIZE` (e.g. `500B`, `2KB`, `10MiB`, `1GB`); smaller files fall back to byte comparison. |
| `--max-size SIZE`        | Only hash-compare files `<= SIZE` (e.g. `100MB`, `2GiB`); larger files fall back to byte comparison.           |
| `--max-find-time TIME`   | Max time allowed for a rename/move search per file (e.g. `5s`, `2m`, `1h`).                                   |
| `-r`, `--find-renamed`   | If a file is missing at its expected path in `DIR2`, search the whole `DIR2` tree for a file with identical content/hash (detects renames or moves). |
| `-p`, `--progress`       | Show a single-line emerge-style progress bar instead of a per-file spinner.                                   |
| `-j`, `--jobs N`         | Number of parallel worker threads (`0` = auto/number of CPUs, default: `1`).                                  |
| `-q`, `--quiet`          | Only print errors and the final summary.                                                                      |
| `-v`, `--verbose`        | Always print full paths instead of paths relative to `DIR1`.                                                  |
| `-L`, `--follow-symlinks`| Follow symlinks while walking directories.                                                                    |
| `--exclude SUBSTR`       | Skip paths whose relative name contains `SUBSTR` (repeatable).                                                |
| `--dry-run`              | Don't check anything, only list what would be checked, then exit.                                             |
| `-h`, `--help`           | Show usage and exit.                                                                                          |
| `-V`, `--version`        | Show version information and exit.                                                                            |

### Hash algorithms

| Name(s) accepted for `-a/--algo`     | Algorithm |
|----------------------------------------|-----------|
| `md5sum`, `md5`                        | MD5 |
| `sha1sum`, `sha1`                      | SHA-1 |
| `sha256sum`, `sha256` *(default)*      | SHA-256 |
| `sha512sum`, `sha512`                  | SHA-512 |
| `sha3-256sum`, `sha3-256`              | SHA3-256 |
| `sha3-512sum`, `sha3-512`              | SHA3-512 |
| `b2sum`, `blake2b`                     | BLAKE2b-512 |
| `b3sum`, `blake3`                      | BLAKE3 |
| `xxh32sum`, `xxh32`                    | xxHash32 |
| `xxh64sum`, `xxh64`                    | xxHash64 |
| `xxh3sum`, `xxh3`                      | XXH3 |
| `crc32sum`, `crc32`                    | CRC-32 |

> [!TIP]
> `crc32sum`/`xxh3sum` are great for a quick "did anything change at all"
> pass over huge trees; `sha256sum`/`b3sum` are the safer choice if you
> actually care about collision resistance.

### Size & time suffixes

`--min-size`, `--max-size` accept plain byte counts or a number followed by
a unit: `b`, `k`/`kb`, `m`/`mb`, `g`/`gb`, `t`/`tb`, `p`/`pb` (decimal, SI),
or `ki`/`kib`, `mi`/`mib`, `gi`/`gib`, `ti`/`tib`, `pi`/`pib` (binary).

`--max-find-time` accepts a plain second count or a number followed by
`s`, `m`, `h`, `d`, `w`.

## Examples

Basic byte-for-byte comparison:

```bash
compareDirs2 /mnt/backup/old /mnt/backup/new
```

Hash-based comparison with BLAKE3:

```bash
compareDirs2 -x -a b3sum /mnt/backup/old /mnt/backup/new
```

Hash only files between 1 MiB and 500 MiB; everything else falls back to
byte comparison (useful when hashing huge files would be too slow, or tiny
files aren't worth the overhead):

```bash
compareDirs2 -x --min-size 1MiB --max-size 500MB DIR1 DIR2
```

Detect files that were renamed or moved instead of being reported missing,
capping the per-file search at 5 seconds:

```bash
compareDirs2 -x -r --max-find-time 5s DIR1 DIR2
```

Run with 8 parallel workers and an emerge-style progress bar:

```bash
compareDirs2 -j 8 -p DIR1 DIR2
```

Skip `.git` and `node_modules`, only show errors and the summary:

```bash
compareDirs2 -q --exclude .git --exclude node_modules DIR1 DIR2
```

Preview what would be checked, without actually checking anything:

```bash
compareDirs2 --dry-run DIR1 DIR2
```

## Output legend

| Status          | Color         | Meaning                                                              |
|-----------------|---------------|-------------------------------------------------------------------------|
| `[ / - \ \| ]`  | white         | File is currently being checked (spinner animation).                 |
| `[ ok ]`        | green         | File matches (byte-for-byte or by hash).                             |
| `[ !! ]`        | red           | File differs, is missing, or was not found anywhere in `DIR2`.       |
| `[ finding / - \ \| ]` | cyan   | Searching `DIR2` for a renamed/moved match (`-r`, spinner animation). |
| `[ found ]`     | green         | A renamed/moved match was located; its path is printed below.        |
| unchecked       | yellow/orange | File was not checked yet — printed on `Ctrl+C` interrupt.            |

Every finished line is prefixed with `*`, matching OpenRC's convention for
completed service actions; in-progress lines have no prefix.

> [!NOTE]
> In `-p/--progress` mode the spinner/per-file lines are replaced entirely
> by a single, continuously updated `Checking (N of TOTAL) [####----] NN% err:E  file` line; errors and rename/found messages are still
> printed below it as they happen.

### Long file names / narrow terminals

The status line adapts to the current terminal width on every redraw:

1. If the full line (name + status) fits — it's printed on one line, as
   normal.
2. If the name alone fits but the status wouldn't — the status is wrapped
   to an indented line below the name.
3. If even the name doesn't fit — it's truncated from the front
   (`...end_of_path.txt`) and kept on one line.

Resizing the terminal mid-run updates this behavior on the fly.

## Interruption (Ctrl+C / Ctrl+Z)

> [!IMPORTANT]
> Pressing `Ctrl+C` stops the spinner/progress bar, prints an
> "interrupted" summary — `OK`, `Errors`, and `Unchecked` counts — and
> exits with status `130`. Pressing `Ctrl+C` a **second time** (after an
> interrupt is already in progress) force-exits immediately.

Pressing `Ctrl+Z` tells background worker/spinner threads to stop touching
the terminal, prints `Paused (Ctrl+Z). Resume with 'fg'.`, then actually
suspends the process via `SIGSTOP`. Resuming with `fg`/`SIGCONT` prints
`Resumed.` and continues exactly where it left off — no garbled terminal
state, unlike the naive default-handler behavior the original Bash script
suffered from.

## Exit codes

| Code  | Meaning                                                       |
|-------|------------------------------------------------------------------|
| `0`   | Comparison (or dry run) finished — see summary for results/errors count. |
| `1`   | Bad arguments, a directory wasn't found, or an unsupported hash algorithm was requested. |
| `130` | Interrupted by `Ctrl+C`.                                          |

## Notes / limitations

- Only regular files under `DIR1` are compared; the tool does not detect
  files that exist in `DIR2` but not in `DIR1`.
- `-r/--find-renamed` performs a linear scan of `DIR2` for every missing
  file, so it can be slow on very large trees — use `--max-find-time` to
  bound the damage.
- Symlinks (as targets), permissions, timestamps, and ownership are **not**
  compared — only file content. `-L/--follow-symlinks` only affects how
  `DIR1`/`DIR2` are *walked*, not what's compared.
- With `-j/--jobs N > 1`, files are checked out of order internally, but
  results are always printed in the same order as a sequential (`-j 1`)
  run — output is deterministic regardless of thread count.

## Requirements

- A Rust toolchain (`cargo build --release`) to build from source.
- No external CLI tools are required at runtime — hashing, byte
  comparison, and directory walking are all done in-process.
>[!important]
>version of rust im using is `1.97.1`
