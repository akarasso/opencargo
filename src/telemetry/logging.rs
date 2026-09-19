//! What the process logs, and in what.

use std::io::IsTerminal;

/// What is logged when `RUST_LOG` says nothing: this crate at info, and
/// nothing from the libraries. `tower_http` was named here beside it, from
/// when a `TraceLayer` logged a line per request; the layer that replaced it
/// records a metric instead, so the directive only stood ready to turn a
/// per-request line back on.
pub const DEFAULT_FILTER: &str = "opencargo=info";

/// Install the subscriber. Colour is a terminal's, and the log of a server is
/// a file, a journal or a container's stdout: 38% of every line of the
/// benchmark's log was escape sequences nothing was going to render.
pub fn init() {
    init_with(std::io::stdout().is_terminal());
}

fn init_with(colour: bool) {
    tracing_subscriber::fmt()
        .with_ansi(colour)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_FILTER.into()),
        )
        .init();
}
