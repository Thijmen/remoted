//! Standalone runner for `rd_platform_macos::selftest` (the real CLI is `rd-agent selftest`).
//!
//! ```text
//! cargo run --release -p rd-platform-macos --example selftest -- --synthetic --size 1728x1118 --fps 60 --seconds 10 --record /tmp/out.rdv
//! ```

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    use rd_platform_macos::RateControl;
    use rd_platform_macos::selftest::{self, SelftestOptions};

    #[derive(Parser)]
    #[command(about = "Capture/encode benchmark with SPS VUI assertions")]
    struct Args {
        /// Output size, WIDTHxHEIGHT (even numbers).
        #[arg(long, default_value = "1728x1118", value_parser = parse_size)]
        size: (u32, u32),
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 10)]
        seconds: u32,
        #[arg(long, default_value_t = 8000)]
        kbps: u32,
        /// Write the stream as an .rdv recording.
        #[arg(long)]
        record: Option<std::path::PathBuf>,
        /// Encode a moving test pattern instead of the screen (needs no permission).
        #[arg(long)]
        synthetic: bool,
        /// VideoToolbox configuration: low-latency or standard.
        #[arg(long, default_value = "standard")]
        rate_control: RateControl,
    }

    fn parse_size(s: &str) -> Result<(u32, u32), String> {
        let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
        let parse = |v: &str| v.parse::<u32>().map_err(|e| format!("{v}: {e}"));
        Ok((parse(w)?, parse(h)?))
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let opts = SelftestOptions {
        width: args.size.0,
        height: args.size.1,
        fps: args.fps,
        seconds: args.seconds,
        bitrate_kbps: args.kbps,
        record: args.record,
        synthetic: args.synthetic,
        rate_control: args.rate_control,
    };
    match selftest::run(&opts) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(e) => {
            eprintln!("selftest error: {e:#}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the selftest only runs on macOS");
}
