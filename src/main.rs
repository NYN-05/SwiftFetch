//! `download-manager` — Windows-first command-line download manager.
//!
//! Phase 0 (repository and build foundation): the executable starts and exits
//! predictably. Download commands and the shared modules (configuration,
//! errors, logging) are introduced by later phases per `DOCS/phases.md`.

/// Composes the startup banner so the entry point is reachable and testable.
fn banner() -> String {
    format!(
        "{} {} (Phase 0 foundation build)",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    )
}

fn main() {
    println!("{}", banner());
}

#[cfg(test)]
mod tests {
    use super::banner;

    #[test]
    fn banner_names_the_application() {
        assert!(
            banner().starts_with("download-manager"),
            "banner should identify the application crate: {:?}",
            banner()
        );
    }
}
