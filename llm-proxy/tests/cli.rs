use std::process::Command;

fn proxy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_llm-proxy"))
}

#[test]
fn preserves_arguments_streams_and_exit() {
    let out = proxy()
        .args([
            "--",
            "sh",
            "-c",
            "printf '%s\\n' \"$1\"; printf 'error detail\\n' >&2; exit 7",
            "sh",
            "spaces; $(do-not-execute)",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "spaces; $(do-not-execute)\n"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("error detail"));
}

#[test]
fn raw_preserves_binary_and_stdin() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = proxy()
        .args(["--raw", "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\0\xff\x1b\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.stdout, b"\0\xff\x1b\n");
}

#[test]
fn drains_both_streams_and_reports_savings() {
    let out = proxy().args(["--stats", "--", "sh", "-c", "i=0; while [ $i -lt 10000 ]; do echo repetitive-output-line; echo repetitive-stderr-line >&2; i=$((i+1)); done; echo ERROR-final >&2; exit 3"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.len() < 3000);
    assert!(out.stderr.len() < 3500);
    assert!(String::from_utf8_lossy(&out.stderr).contains("ERROR-final"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("reduction"));
}

#[test]
fn invalid_options_and_missing_command() {
    assert!(!proxy()
        .args(["--lines", "0", "ls"])
        .status()
        .unwrap()
        .success());
    assert_eq!(
        proxy()
            .args(["--", "llm-proxy-no-such-program"])
            .status()
            .unwrap()
            .code(),
        Some(127)
    );
}

#[test]
fn shell_hook_is_opt_in_and_preserves_quotes() {
    let init = proxy().args(["init", "bash"]).output().unwrap();
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_llm-proxy"))
        .parent()
        .unwrap();
    let script = format!(
        "{}\nexport LLM_PROXY=1\ncat -- /this-file-does-not-exist",
        String::from_utf8_lossy(&init.stdout)
    );
    let out = Command::new("bash")
        .args(["-c", &script])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("llm-proxy: command exited 1"));
}
