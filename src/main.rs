//! lk-latency: LiveKit video latency harness.
//!
//! One binary contains the publisher, the viewer (headless or windowed), and the
//! orchestrator. The orchestrator re-executes this same binary as
//! `lk-latency publisher …` / `lk-latency subscriber …`, so every measurement in a
//! run, native or in Docker, uses identical code.

#[cfg(lk_argus)]
mod argus;
mod codec_display;
mod connection_info;
mod data_probe;
mod headless;
mod orchestrator;
mod publish_timing_log;
mod publisher;
mod subscriber;
mod subscriber_timing;
mod test_pattern;
mod timestamp_burn;
mod timing_csv;
mod user_data;
mod video_display;
mod viewport_aspect;

use std::ffi::OsString;

/// Upstream rust-sdks commit this binary is built against (from Cargo.toml).
pub const SDK_REV: &str = env!("LK_LATENCY_SDK_REV");
/// Source commit of this harness, when known at build time.
pub const BUILD_COMMIT: &str = env!("LK_LATENCY_COMMIT");

fn main() {
    install_logger();
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    let role = argv.get(1).and_then(|arg| arg.to_str()).unwrap_or_default().to_owned();
    let result = match role.as_str() {
        // Internal roles: what the orchestrator launches. Usable directly as well.
        "publisher" | "subscriber" => {
            argv.remove(0);
            argv[0] = format!("lk-latency {role}").into();
            if role == "publisher" {
                publisher::run_main(argv)
            } else {
                subscriber::run_main(argv)
            }
        }
        _ => orchestrator::main(argv),
    };
    if let Err(error) = result {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

/// env_logger, plus a one-line report of the server region and node.
///
/// The SDK logs the full JoinResponse at debug level; we always observe that one
/// record and print `LiveKit server: region=… node_id=…` without enabling any
/// other debug output. This avoids patching the SDK.
struct Logger {
    inner: env_logger::Logger,
}

const JOIN_TARGET: &str = "livekit::rtc_engine::rtc_session";

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
            || (metadata.target() == JOIN_TARGET && metadata.level() <= log::Level::Debug)
    }

    fn log(&self, record: &log::Record) {
        if record.target() == JOIN_TARGET {
            let message = record.args().to_string();
            if message.starts_with("received JoinResponse") {
                if let Some(server) = message.split("server_info: Some(ServerInfo {").nth(1) {
                    let field = |name: &str| {
                        server
                            .split(&format!("{name}: \""))
                            .nth(1)
                            .and_then(|rest| rest.split('"').next())
                            .unwrap_or("?")
                            .to_owned()
                    };
                    println!("LiveKit server: region={} node_id={}", field("region"), field("node_id"));
                }
            }
        }
        if self.inner.enabled(record.metadata()) {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

fn install_logger() {
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build();
    let max = inner.filter().max(log::LevelFilter::Debug);
    if log::set_boxed_logger(Box::new(Logger { inner })).is_ok() {
        log::set_max_level(max);
    }
}
