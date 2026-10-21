use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};
use std::thread;

const HELP: &str = "llm-proxy [--stats] [--lines N] [--raw] -- COMMAND [ARGS...]\nllm-proxy init bash|zsh\n\nRuns commands without shell evaluation. Default: 60 lines per stream.\n--raw bypasses filtering (binary, interactive, or machine-readable output).\n--stats reports byte savings and approximate tokens (bytes / 4) to stderr.\ninit prints opt-in shell functions; see README for installation and limitations.";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    General,
    Files,
    Git,
    Checks,
}

fn mode(args: &[OsString]) -> Mode {
    let name = std::path::Path::new(&args[0])
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let words: Vec<_> = args.iter().skip(1).map(|s| s.to_string_lossy()).collect();
    match name.as_ref() {
        "ls" | "cat" | "grep" | "rg" | "find" => Mode::Files,
        "git" => Mode::Git,
        "pytest" | "ruff" | "eslint" | "tsc" => Mode::Checks,
        "cargo"
            if words
                .iter()
                .any(|w| matches!(w.as_ref(), "test" | "clippy" | "check")) =>
        {
            Mode::Checks
        }
        "npm" | "pnpm" | "yarn" | "bun"
            if words
                .iter()
                .any(|w| matches!(w.as_ref(), "test" | "lint" | "typecheck")) =>
        {
            Mode::Checks
        }
        "python" | "python3" if words.windows(2).any(|w| w[0] == "-m" && w[1] == "pytest") => {
            Mode::Checks
        }
        _ => Mode::General,
    }
}

fn important(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    [
        "error",
        "fail",
        "panic",
        "traceback",
        "exception",
        "warning",
        "assert",
        "fatal",
        "caused by:",
        "test result:",
        "short test summary",
        "tests:",
        "✖",
        "×",
    ]
    .iter()
    .any(|w| s.contains(w))
}

// Strip terminal control sequences, including OSC hyperlinks, before matching.
fn clean(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 27 {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
            } else if i < bytes.len() && bytes[i] == b']' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 7 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 27 && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i = (i + 1).min(bytes.len());
            }
        } else {
            if bytes[i] == b'\t' || bytes[i] >= 32 {
                out.push(bytes[i]);
            }
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Capture {
    head: Vec<(usize, String)>,
    tail: VecDeque<(usize, String)>,
    focus: VecDeque<(usize, String)>,
    bytes: usize,
    lines: usize,
    limit: usize,
}

fn push(queue: &mut VecDeque<(usize, String)>, item: (usize, String), cap: usize) {
    if queue.len() == cap {
        queue.pop_front();
    }
    queue.push_back(item);
}

fn capture<R: io::Read>(reader: R, limit: usize, mode: Mode) -> io::Result<Capture> {
    let mut r = BufReader::new(reader);
    let mut c = Capture {
        head: Vec::new(),
        tail: VecDeque::new(),
        focus: VecDeque::new(),
        bytes: 0,
        lines: 0,
        limit,
    };
    let mut line = Vec::new();
    let mut truncated = false;
    let mut context = 0;
    let mut previous = None;
    let mut skip_lf = false;
    loop {
        let buf = r.fill_buf()?;
        if skip_lf {
            skip_lf = false;
            if buf.first() == Some(&b'\n') {
                c.bytes += 1;
                r.consume(1);
                continue;
            }
        }
        let eof = buf.is_empty();
        let n = buf
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .map(|n| n + 1)
            .unwrap_or(buf.len());
        let ended = n > 0 && matches!(buf[n - 1], b'\n' | b'\r');
        skip_lf = ended && buf[n - 1] == b'\r';
        let keep = n.min(4096usize.saturating_sub(line.len()));
        line.extend_from_slice(&buf[..keep]);
        truncated |= keep < n;
        c.bytes += n;
        r.consume(n);
        if ended || eof {
            if !line.is_empty() || truncated {
                let mut s = clean(&line);
                if truncated {
                    s.push_str(" … [line truncated]");
                }
                if !s.trim().is_empty() || matches!(mode, Mode::Files | Mode::Git) {
                    c.lines += 1;
                    let item = (c.lines, s);
                    if c.head.len() < limit {
                        c.head.push(item.clone());
                    }
                    push(&mut c.tail, item.clone(), limit);
                    let git_header = mode == Mode::Git
                        && [
                            "diff --git",
                            "@@",
                            "On branch",
                            "Changes ",
                            "Untracked files:",
                            "CONFLICT",
                            "commit ",
                        ]
                        .iter()
                        .any(|p| item.1.starts_with(p));
                    let text = item.1.trim();
                    let passing = mode == Mode::Checks
                        && ((text.starts_with("test ") && text.ends_with(" ... ok"))
                            || (text.contains("::")
                                && text.split_whitespace().any(|w| w == "PASSED"))
                            || text.starts_with("PASS ")
                            || text.starts_with('✓'));
                    if passing {
                        context = 0;
                        previous = None;
                    } else {
                        if important(&item.1) || git_header {
                            if let Some(prev) = previous.take() {
                                push(&mut c.focus, prev, limit);
                            }
                            context = 5;
                        }
                        if context > 0 {
                            push(&mut c.focus, item.clone(), limit);
                            context -= 1;
                        }
                        previous = Some(item);
                    }
                }
                line.clear();
                truncated = false;
            }
            if eof {
                break;
            }
        }
    }
    Ok(c)
}

impl Capture {
    fn render(&self, mode: Mode, failed: bool) -> String {
        if self.lines == 0 {
            return String::new();
        }
        let mut selected = Vec::new();
        if mode != Mode::Checks && self.lines <= self.limit {
            selected.extend(self.head.iter().cloned());
        } else if mode == Mode::Checks {
            selected.extend(self.focus.iter().cloned());
            if failed {
                selected.extend(self.tail.iter().rev().take(self.limit / 3).cloned());
            } else if selected.is_empty() {
                return format!("[checks passed; {} output lines suppressed]\n", self.lines);
            }
        } else {
            selected.extend(self.head.iter().take(self.limit / 2).cloned());
            selected.extend(self.focus.iter().cloned());
            selected.extend(self.tail.iter().rev().take(self.limit / 3).cloned());
        }
        selected.sort_by_key(|x| x.0);
        selected.dedup_by_key(|x| x.0);
        if selected.len() > self.limit {
            // Keep the start and end of the selected diagnostics.
            selected.drain(self.limit / 2..selected.len() - (self.limit - self.limit / 2));
        }
        let mut out = String::new();
        let mut last = 0;
        for (number, line) in selected {
            if number > last + 1 {
                out.push_str(&format!("[… {} lines omitted]\n", number - last - 1));
            }
            out.push_str(&line);
            out.push('\n');
            last = number;
        }
        if last < self.lines {
            out.push_str(&format!("[… {} lines omitted]\n", self.lines - last));
        }
        out
    }
}

fn shell_init(shell: &str) -> Result<(), String> {
    if !matches!(shell, "bash" | "zsh") {
        return Err("supported shells: bash, zsh".into());
    }
    println!("# Enable with: export LLM_PROXY=1; disable with: unset LLM_PROXY");
    for cmd in [
        "ls", "cat", "grep", "rg", "find", "git", "pytest", "cargo", "npm", "pnpm", "yarn", "bun",
        "ruff", "eslint", "tsc",
    ] {
        println!("{cmd}() {{ if [ \"${{LLM_PROXY:-0}}\" = 1 ]; then command llm-proxy -- {cmd} \"$@\"; else command {cmd} \"$@\"; fi; }}");
    }
    Ok(())
}

fn run() -> Result<i32, String> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        println!("{HELP}");
        return Ok(0);
    }
    if args[0] == "--version" {
        println!("llm-proxy {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if args[0] == "init" {
        if args.len() != 2 {
            return Err("usage: llm-proxy init bash|zsh".into());
        }
        shell_init(&args[1].to_string_lossy())?;
        return Ok(0);
    }
    let (mut stats, mut raw, mut limit, mut i) = (false, false, 60, 0);
    while i < args.len() {
        match args[i].to_str() {
            Some("--") => {
                i += 1;
                break;
            }
            Some("--stats") => stats = true,
            Some("--raw") => raw = true,
            Some("--lines") => {
                i += 1;
                limit = args
                    .get(i)
                    .and_then(|x| x.to_str())
                    .and_then(|x| x.parse::<usize>().ok())
                    .filter(|n| (6..=10000).contains(n))
                    .ok_or("--lines must be 6..10000")?;
            }
            Some(s) if s.starts_with('-') => return Err(format!("unknown option: {s}")),
            _ => break,
        }
        i += 1;
    }
    let args = &args[i..];
    if args.is_empty() {
        return Err("missing command".into());
    }
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    if raw {
        return cmd
            .status()
            .map(status_code)
            .map_err(|e| format!("cannot run {:?}: {e}", args[0]));
    }
    let kind = mode(args);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {:?}: {e}", args[0]))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let a = thread::spawn(move || capture(stdout, limit, kind));
    let b = thread::spawn(move || capture(stderr, limit, kind));
    let status = child.wait().map_err(|e| e.to_string())?;
    let a = a
        .join()
        .map_err(|_| "stdout reader panicked")?
        .map_err(|e| e.to_string())?;
    let b = b
        .join()
        .map_err(|_| "stderr reader panicked")?
        .map_err(|e| e.to_string())?;
    let out = a.render(kind, !status.success());
    let err = b.render(kind, !status.success());
    write_output(&mut io::stdout().lock(), out.as_bytes())?;
    write_output(&mut io::stderr().lock(), err.as_bytes())?;
    if !status.success() {
        eprintln!("[llm-proxy: command exited {}]", status_code(status));
    }
    if stats {
        let input = a.bytes + b.bytes;
        let output = out.len() + err.len();
        let saved = if input == 0 {
            0.0
        } else {
            100.0 * (1.0 - output as f64 / input as f64)
        };
        eprintln!("[llm-proxy: {input} → {output} bytes; {saved:.1}% reduction; ~{} → ~{} tokens (bytes/4 estimate; excludes metadata)]", input.div_ceil(4), output.div_ceil(4));
    }
    Ok(status_code(status))
}

fn write_output(w: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    match w.write_all(bytes) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
fn status_code(s: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        s.code().unwrap_or(128 + s.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        s.code().unwrap_or(1)
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("llm-proxy: {e}");
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_output_keeps_failure_context() {
        let input = format!(
            "{}\nFAILED test_checkout\n  expected 2, got 1\n{}",
            "test ok\n".repeat(10000),
            "test ok\n".repeat(10000)
        );
        let c = capture(input.as_bytes(), 60, Mode::Checks).unwrap();
        let out = c.render(Mode::Checks, true);
        assert!(out.contains("FAILED test_checkout"));
        assert!(out.contains("expected 2, got 1"));
        assert!(out.len() < input.len() / 10);
        assert!(c.head.len() <= 60 && c.tail.len() <= 60 && c.focus.len() <= 60);
    }
    #[test]
    fn huge_unicode_line_is_bounded() {
        let input = "🦀".repeat(100000);
        let c = capture(input.as_bytes(), 6, Mode::Files).unwrap();
        assert_eq!(c.bytes, input.len());
        assert!(c.render(Mode::Files, false).contains("line truncated"));
    }
    #[test]
    fn strips_terminal_escapes() {
        assert_eq!(
            clean(b"\x1b[31merror\x1b[0m \x1b]8;;https://example.com\x07link\x1b]8;;\x1b\\"),
            "error link"
        );
    }
    #[test]
    fn failed_checks_without_keywords_keep_tail() {
        let c = capture(b"setup\nsegmentation fault\n".as_slice(), 60, Mode::Checks).unwrap();
        assert!(c.render(Mode::Checks, true).contains("segmentation fault"));
    }
    #[test]
    fn passing_test_names_do_not_trigger_failure_filter() {
        let input = "test tests::failure_and_error ... ok\ntest_sample.py::test_error PASSED [100%]\nPASS error.test.js\n";
        let c = capture(input.as_bytes(), 60, Mode::Checks).unwrap();
        assert_eq!(
            c.render(Mode::Checks, false),
            "[checks passed; 3 output lines suppressed]\n"
        );
    }
    #[test]
    fn small_file_output_is_complete() {
        let input = (0..59).map(|n| format!("line {n}\n")).collect::<String>();
        assert_eq!(
            capture(input.as_bytes(), 60, Mode::Files)
                .unwrap()
                .render(Mode::Files, false),
            input
        );
    }
    #[test]
    fn detects_supported_runners_and_git_headers() {
        for words in [
            vec!["python3", "-m", "pytest"],
            vec!["npm", "run", "lint"],
            vec!["cargo", "test"],
            vec!["ruff", "check"],
        ] {
            let args: Vec<OsString> = words.iter().map(OsString::from).collect();
            assert!(mode(&args) == Mode::Checks);
        }
        let input = format!(
            "{}diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n{}",
            "noise\n".repeat(1000),
            "noise\n".repeat(1000)
        );
        let out = capture(input.as_bytes(), 60, Mode::Git)
            .unwrap()
            .render(Mode::Git, false);
        assert!(out.contains("diff --git") && out.contains("+new"));
    }
    #[test]
    fn preserves_blank_file_and_diff_lines() {
        for mode in [Mode::Files, Mode::Git] {
            let input = "\nfirst\n\n \n\t\nlast\n\n";
            let c = capture(input.as_bytes(), 60, mode).unwrap();
            assert_eq!(c.render(mode, false), input);
            assert_eq!(c.lines, 7);
        }
    }

    #[test]
    fn normalizes_crlf_even_across_read_boundaries() {
        struct OneByte<'a>(&'a [u8]);
        impl io::Read for OneByte<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = buf.len().min(1).min(self.0.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let input = b"first\r\n\r\n \r\nlast\rnext\n";
        for mode in [Mode::Files, Mode::Git] {
            let c = capture(OneByte(input), 60, mode).unwrap();
            assert_eq!(c.bytes, input.len());
            assert_eq!(c.render(mode, false), "first\n\n \nlast\nnext\n");
            assert_eq!(c.lines, 5);
        }
    }
}
