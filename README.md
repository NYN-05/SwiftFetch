# SwiftFetch

SwiftFetch is a lightweight, Windows-first command-line download manager in
Rust. It accepts
one URL or a list of URLs, downloads files efficiently and reliably, resumes
interrupted downloads, and automatically extracts completed ZIP archives.

This project is developed in the controlled phases defined in
[`DOCS/phases.md`](DOCS/phases.md). The current build implements **Phase 1 —
CLI Shell, Configuration, Errors, and Logging**: the command surface and the
shared infrastructure are in place; downloading is not implemented yet.

- Product requirements: [`DOCS/PRD.md`](DOCS/PRD.md)
- Architecture: [`DOCS/architecture.md`](DOCS/architecture.md)
- Running changelog (all code/feature changes): [`changes.md`](changes.md)
- Architecture decision record: [`decision.md`](decision.md)

## CLI (Phase 1 preview)

Try `swift-fetch --help`. The command surface (`download`, `status`,
`resume`, `retry`) and shared configuration, error, and logging foundations
exist, but no command is wired to a transfer engine yet: every command exits
with an honest not-implemented error (exit code 1; invalid invocations exit 2).

## Target platform

Windows is the first target platform. Development currently runs on Windows;
Windows-specific behavior is verified on Windows.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable) with `cargo`
- The `rustfmt` and `clippy` toolchain components

## Build and test commands

From the repository root, in this order:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Build the executable:

```sh
cargo build
```

Running the development check sequence, building, and testing from a clean
checkout is verified during Phase 0; use `git clone` plus the commands above to
reproduce it.
