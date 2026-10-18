# download-manager

A lightweight, Windows-first command-line download manager in Rust. It accepts
one URL or a list of URLs, downloads files efficiently and reliably, resumes
interrupted downloads, and automatically extracts completed ZIP archives.

This project is developed in the controlled phases defined in
[`DOCS/phases.md`](DOCS/phases.md). The current build implements **Phase 0 —
Repository and Build Foundation**: the package builds and tests reproducibly,
and the executable starts and exits predictably. Download behavior is not
implemented yet.

- Product requirements: [`DOCS/PRD.md`](DOCS/PRD.md)
- Architecture: [`DOCS/architecture.md`](DOCS/architecture.md)
- Running changelog (all code/feature changes): [`changes.md`](changes.md)
- Architecture decision record: [`decision.md`](decision.md)

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
