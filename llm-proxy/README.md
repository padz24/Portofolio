# llm-proxy

A small Rust command proxy that reduces developer command output before it enters a coding agent's context. No crates, network calls, API keys, or runtime services. The underlying commands still need to be installed.

## Build and install

Requires Rust/Cargo 1.73+ to build. From this directory:

```sh
cargo build --release --offline
install -Dm755 target/release/llm-proxy "$HOME/.local/bin/llm-proxy"
export PATH="$HOME/.local/bin:$PATH"
```

For a fully static Linux executable with the installed GNU Rust toolchain:

```sh
RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --offline
```

Alternatively, with rustup and a musl linker installed, add the `x86_64-unknown-linux-musl` target and build for it. Distribute the resulting binary for that platform; recipients need no Rust installation. A normal build may depend on the platform's C runtime.

## Usage

```sh
llm-proxy --stats -- ls -R .
llm-proxy -- git status
llm-proxy -- git diff
llm-proxy -- cat src/main.rs
llm-proxy -- grep -rn TODO src
llm-proxy -- pytest -v
llm-proxy -- cargo test
llm-proxy -- npm test
llm-proxy --lines 120 -- cargo clippy
llm-proxy --raw -- git diff --binary
```

Arguments are passed directly to the executable, without shell expansion or evaluation. Stdin, environment, and current directory are inherited. Stdout and stderr remain separate, and the child exit code is preserved (Unix signals become `128 + signal`; Windows exit codes outside 0–255 map to 1). Proxy errors use exit code 127. Use `sh -c '...'` explicitly if you need shell syntax.

| Commands | Output policy |
| --- | --- |
| `ls`, `cat`, `grep`, `rg`, `find` | Keep small outputs intact; sample the beginning/end of large outputs and retain diagnostic context. |
| `git` | Also prioritize status sections, conflicts, commit headers, and diff hunk headers. Original command arguments and behavior are preserved. |
| `pytest`, `python -m pytest`, `cargo test/check/clippy`, npm/pnpm/yarn/bun `test/lint/typecheck`, `ruff`, `eslint`, `tsc` | Suppress successful chatter; retain error/failure/warning markers with adjacent lines and result summaries. On failure, retain the tail even without recognized markers. |
| Other commands | Bounded beginning/end and diagnostic context. |

Filtering is heuristic, not a semantic parser. Test aliases with custom names use the general filter. Diagnostic budgets can omit failures in very large failing runs; omission markers tell the agent when to rerun a narrower command or use `--raw`. Checks emit a short success summary instead of individual passing tests. Small listings and source files often have little reducible content. An 80–90% reduction is a target for verbose/repetitive workloads, not a universal guarantee. `--stats` reports actual captured byte reduction and a **rough bytes/4 token estimate**, not provider billing counts. Metadata is excluded from the estimate.

## Bash and Zsh integration

Add to `~/.bashrc` or `~/.zshrc` after putting the binary on PATH:

```sh
eval "$(llm-proxy init bash)" # use zsh for Zsh
export LLM_PROXY=1
```

This installs functions for the common commands listed above (except Python). Enable only in a shell dedicated to agent-facing output. Existing functions with these names are replaced; existing aliases may take precedence and should be removed manually. Disable filtering with `unset LLM_PROXY`, bypass one function with `command git status`, or use `llm-proxy --raw -- COMMAND`.

**Filtered output is lossy:** do not use it for patches, JSON, binary files, program inputs, or pipelines that depend on exact output. For example, use `command cat file | parser`. Hooks also filter redirected output. Shell functions cannot intercept subprocesses launched directly by an agent; configure that agent to prefix its command executions with `llm-proxy --`. No shell configuration is modified automatically.

## Performance and boundaries

Both streams are consumed concurrently. Memory is bounded by the line budget, with at most 4096 bytes retained per input line and three bounded buffers per stream. Processing is linear in input size. The default budget is 60 selected lines per stream, plus omission notices. ANSI color and OSC terminal escapes are removed; invalid UTF-8 is replaced. Output appears when the process finishes, so use `--raw` for interactive programs, watch modes, progress streaming, pagers, and commands that require a TTY. Capturing pipes changes TTY-sensitive child behavior. The proxy does not sandbox commands or redact secrets.

## Verification

```sh
cargo test --offline
cargo fmt --check
cargo clippy --offline --all-targets -- -D warnings
```

Tests cover bounded output, failure context, Unicode/huge lines, terminal escapes, argument fidelity, exit status, concurrent stdout/stderr, raw binary/stdin passthrough, and Bash integration.
