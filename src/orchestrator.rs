//! `run`, `publish`, `view`, `summarize`, `version`: launch publisher/viewer
//! processes (this same binary), apply optional container firewall rules, and
//! turn their timing CSVs into per-stage latency summaries.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, TcpStream, ToSocketAddrs, UdpSocket},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_RUST_LOG: &str = "warn,lk_latency=info";
const STATIC_CIDRS: &str = include_str!("../cloud-static-ips.txt");

/// Regions whose endpoint does not follow `<project>.<region>.rtc.livekit.cloud`.
const REGION_HOSTS: &[(&str, &str)] = &[("uae", "{project}.odubai1a.production.livekit.cloud")];
/// Server-reported region names accepted for each requested region slug.
const REGION_ALIASES: &[(&str, &[&str])] = &[
    ("uae", &["uae", "dubai"]),
    ("eu", &["eu", "europe", "france", "germany"]),
    ("us", &["us", "united states"]),
    ("canada", &["canada"]),
    ("india", &["india"]),
    ("japan", &["japan"]),
];

const PUBLISHER_STAGES: &[&str] = &["EncoderUpload", "EncoderOutput", "WebrtcPacketize"];
/// (key, label, from stage, to stage); "capture" is the frame's embedded capture time.
/// DecoderUpload is observed in the frame transformer, before libwebrtc's frame
/// buffer, so the jitter-buffer wait is inside `buffer_and_decode`.
const SEGMENTS: &[(&str, &str, &str, &str)] = &[
    ("capture_to_encoder", "capture -> encoder", "capture", "EncoderUpload"),
    ("encode", "encode", "EncoderUpload", "EncoderOutput"),
    ("encoder_to_packetize", "encoder -> packetize", "EncoderOutput", "WebrtcPacketize"),
    ("network", "network (packetize -> receive)", "WebrtcPacketize", "WebrtcReceive"),
    ("receive_transform", "receive transform", "WebrtcReceive", "DecoderUpload"),
    ("buffer_and_decode", "jitter buffer + decode", "DecoderUpload", "DecoderOutput"),
    ("delivery", "decoded -> app", "DecoderOutput", "FrameSink"),
    ("capture_to_receive", "capture -> receive", "capture", "WebrtcReceive"),
    ("total", "TOTAL capture -> decoded", "capture", "FrameSink"),
];

// ---------------------------------------------------------------------------
// CLI

#[derive(Parser)]
#[command(name = "lk-latency", version, about = "LiveKit video latency harness", after_help = AFTER_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Mode,
}

const AFTER_HELP: &str = "Credentials come from LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET.\n\
All processes in a measurement must share a clock (one host, or one Docker VM).\n\
Internal roles: `lk-latency publisher --help`, `lk-latency subscriber --help`.";

#[derive(Subcommand)]
enum Mode {
    /// Publisher + viewers in one go, repeated, with a summary
    Run(RunArgs),
    /// Publisher only (pair with `view`)
    Publish(PublishArgs),
    /// Viewer only
    View(ViewArgs),
    /// Analyze an existing results directory
    Summarize { directory: PathBuf },
    /// Print build and binary identity
    Version,
}

#[derive(Args, Clone)]
struct RunArgs {
    /// Region slug (eu, us, india, japan, canada, uae, ...), `default`, or a wss:// URL
    #[arg(long, default_value = "default")]
    publisher_region: String,
    /// Viewer endpoint (same forms as --publisher-region); repeat for simultaneous viewers
    #[arg(long = "viewer-region")]
    viewer_regions: Vec<String>,
    /// Repeat with a fresh room and fresh connections
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=1000))]
    iterations: u32,
    /// Seconds between iterations
    #[arg(long, default_value_t = 2.0)]
    pause: f64,
    /// Results subdirectory name (default: timestamped)
    #[arg(long)]
    run_id: Option<String>,
    #[command(flatten)]
    video: VideoArgs,
    #[command(flatten)]
    viewer: ViewerArgs,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Clone)]
struct PublishArgs {
    #[arg(long)]
    room: String,
    /// Region slug, `default`, or a wss:// URL
    #[arg(long, default_value = "default")]
    region: String,
    /// Stop after N seconds (default: until Ctrl-C / docker stop)
    #[arg(long)]
    duration: Option<u64>,
    #[command(flatten)]
    video: VideoArgs,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Clone)]
struct ViewArgs {
    #[arg(long)]
    room: String,
    /// Region slug, `default`, or a wss:// URL
    #[arg(long, default_value = "default")]
    region: String,
    /// Viewer identity and file prefix (default: viewer-<region>)
    #[arg(long)]
    name: Option<String>,
    /// Publisher identity to watch
    #[arg(long, default_value = "lk-latency-pub")]
    identity: String,
    #[command(flatten)]
    viewer: ViewerArgs,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Clone)]
struct VideoArgs {
    /// Video source: synthetic test pattern, or a local camera (native runs)
    #[arg(long, value_enum, default_value_t = Source::Pattern)]
    source: Source,
    /// Camera index for --source camera (see `lk-latency publisher --list-cameras`)
    #[arg(long, default_value_t = 0)]
    camera_index: usize,
    /// Test pattern: moving (realistic encoder load) or static bars
    #[arg(long, value_enum, default_value_t = Pattern::Moving)]
    pattern: Pattern,
    #[arg(long, value_enum, default_value_t = Codec::Vp8)]
    codec: Codec,
    /// software (portable default), auto, hardware, videotoolbox (macOS), nvenc, vaapi
    #[arg(long, default_value = "software")]
    encoder: String,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// Max video bitrate in bps
    #[arg(long)]
    max_bitrate: Option<u64>,
    #[arg(long)]
    simulcast: bool,
    /// Burn the capture timestamp into the video
    #[arg(long)]
    burn_timestamp: bool,
    /// Publisher identity
    #[arg(long, default_value = "lk-latency-pub")]
    identity: String,
    #[arg(long, default_value_t = 0)]
    min_playout_delay: u32,
    /// Room max playout delay in ms, applied by recreating the room; `server` leaves
    /// the room configuration untouched
    #[arg(long, default_value = "10")]
    max_playout_delay: String,
}

#[derive(Args, Clone)]
struct ViewerArgs {
    /// Measured seconds per viewer, after warm-up
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    duration: u64,
    #[arg(long, default_value_t = 3)]
    warmup: u64,
    #[arg(long, default_value_t = 45)]
    startup_timeout: u64,
    /// Force zero playout delay on the viewer (overrides the room setting)
    #[arg(long)]
    low_latency: bool,
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Show windows: publisher preview and viewer video with timestamp overlay
    /// (native runs only; not available in Docker)
    #[arg(long)]
    display: bool,
    /// Restrict this container's egress (needs --cap-add NET_ADMIN)
    #[arg(long, value_enum, default_value_t = Firewall::Open)]
    firewall_profile: Firewall,
    /// Allow this IPv4 on TCP/443 under a firewall profile; repeatable
    #[arg(long = "signaling-ip")]
    signaling_ips: Vec<Ipv4Addr>,
    /// Before media, count unsolicited UDP sent to port 3478 for SECONDS
    #[arg(long, value_name = "SECONDS", default_value_t = 0)]
    inbound_udp_check: u64,
    /// Require TURN relay candidates
    #[arg(long)]
    relay_only: bool,
    /// Also exercise reliable/lossy data round trips
    #[arg(long)]
    data_probe: bool,
    #[arg(long, env = "LK_LATENCY_OUTPUT", default_value = "results")]
    output: PathBuf,
    /// Echo all child process output
    #[arg(long)]
    verbose: bool,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Debug)]
enum Source {
    Pattern,
    Camera,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Debug)]
enum Pattern {
    Moving,
    Bars,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Debug)]
enum Codec {
    Vp8,
    Vp9,
    Av1,
    H264,
    H265,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Debug)]
enum Firewall {
    Open,
    StaticFull,
    StaticUdp,
    StaticTurnUdp,
    StaticTcp,
    StaticTls,
    OutboundTurnUdp,
}

fn value_name<T: ValueEnum>(value: T) -> String {
    value.to_possible_value().map(|v| v.get_name().to_owned()).unwrap_or_default()
}

pub fn main(argv: Vec<OsString>) -> Result<()> {
    let cli = Cli::parse_from(argv);
    let code = match cli.command {
        Mode::Version => {
            print!("{}", version_text()?);
            0
        }
        Mode::Summarize { directory } => mode_summarize(&directory)?,
        Mode::Run(args) => {
            handle_signals();
            mode_run(args)?
        }
        Mode::Publish(args) => {
            handle_signals();
            mode_publish(args)?
        }
        Mode::View(args) => {
            handle_signals();
            mode_view(args)?
        }
    };
    std::process::exit(code)
}

fn log(message: impl AsRef<str>) {
    println!("[lk-latency] {}", message.as_ref());
}

// ---------------------------------------------------------------------------
// Build identity

fn version_text() -> Result<String> {
    let exe = std::env::current_exe()?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut file = fs::File::open(&exe)?;
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buffer[..n]);
    }
    let digest = sha2::Digest::finalize(hasher);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "lk-latency {}\ncommit: {}\nlivekit rust-sdks: {}\ntarget: {}-{}\nbinary sha256: {}\n",
        env!("CARGO_PKG_VERSION"),
        crate::BUILD_COMMIT,
        crate::SDK_REV,
        std::env::consts::OS,
        std::env::consts::ARCH,
        hex
    ))
}

fn build_info() -> Value {
    let text = version_text().unwrap_or_default();
    let field = |name: &str| {
        text.lines().find_map(|l| l.strip_prefix(&format!("{name}: "))).unwrap_or("?").to_owned()
    };
    json!({
        "version": env!("CARGO_PKG_VERSION"), "commit": crate::BUILD_COMMIT, "sdk_rev": crate::SDK_REV,
        "target": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "binary_sha256": field("binary sha256"), "in_docker": Path::new("/.dockerenv").exists(),
    })
}

// ---------------------------------------------------------------------------
// Endpoints and regions

fn resolve_endpoint(spec: &str, base_url: &str) -> Result<String> {
    if spec.is_empty() || spec == "default" {
        return Ok(base_url.to_owned());
    }
    if spec.contains("://") {
        if !(spec.starts_with("ws://") || spec.starts_with("wss://")) {
            bail!("Endpoint URL must be ws:// or wss://: {spec}");
        }
        return Ok(spec.to_owned());
    }
    if !spec.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        bail!("Invalid region slug: {spec:?}");
    }
    let host = base_url
        .strip_prefix("wss://")
        .map(|rest| rest.split(['/', '?']).next().unwrap_or(rest))
        .filter(|host| host.ends_with(".livekit.cloud"))
        .ok_or_else(|| {
            anyhow!("Region slugs need a LiveKit Cloud LIVEKIT_URL (wss://<project>.livekit.cloud); pass a full wss:// URL instead")
        })?;
    let project = host.split('.').next().unwrap_or_default();
    let template = REGION_HOSTS
        .iter()
        .find(|(name, _)| *name == spec)
        .map(|(_, t)| *t)
        .unwrap_or("{project}.{region}.rtc.livekit.cloud");
    Ok(format!("wss://{}", template.replace("{project}", project).replace("{region}", spec)))
}

/// A label safe to share: never includes the project hostname.
fn describe_endpoint(spec: &str) -> String {
    match spec {
        "" | "default" => "default".into(),
        s if s.contains("://") => "custom-url".into(),
        s => s.into(),
    }
}

fn expected_region(spec: &str) -> Option<String> {
    REGION_ALIASES.iter().any(|(name, _)| *name == spec).then(|| spec.to_owned())
}

fn region_matches(actual: &str, expected: &str) -> bool {
    let actual = actual.trim().to_lowercase();
    let aliases = REGION_ALIASES.iter().find(|(name, _)| *name == expected).map(|(_, a)| a.to_vec());
    aliases.unwrap_or_else(|| vec![expected]).iter().any(|name| {
        actual == *name || actual.starts_with(&format!("{name} ")) || actual.starts_with(&format!("{name}-"))
    })
}

fn hostname(url: &str) -> Option<&str> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split(['/', '?']).next()?;
    Some(host.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map_or(host, |(h, _)| h))
}

// ---------------------------------------------------------------------------
// Container firewall (requires --cap-add NET_ADMIN)

fn static_cidrs() -> Vec<&'static str> {
    STATIC_CIDRS.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

fn resolve_ipv4<'a>(urls: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut addresses = BTreeSet::new();
    for url in urls {
        if let Some(host) = hostname(url) {
            if let Ok(resolved) = (host, 443).to_socket_addrs() {
                addresses.extend(resolved.filter(|a| a.is_ipv4()).map(|a| a.ip().to_string()));
            }
        }
    }
    addresses.into_iter().collect()
}

fn apply_firewall(profile: Firewall, signaling: &[String], out: &Path) -> Result<()> {
    if profile == Firewall::Open {
        return Ok(());
    }
    let rule = |command: &str, args: &[&str]| -> Result<()> {
        let output = Command::new(command).arg("-w").args(args).output().with_context(|| {
            format!("{command} not found; firewall profiles run inside the Docker image")
        })?;
        if !output.status.success() {
            bail!(
                "{command} failed ({}). Firewall profiles need `docker run --cap-add NET_ADMIN`.",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    };
    for command in ["iptables", "ip6tables"] {
        rule(command, &["-P", "INPUT", "DROP"])?;
        rule(command, &["-A", "INPUT", "-i", "lo", "-j", "ACCEPT"])?;
        rule(command, &["-A", "INPUT", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"])?;
        rule(command, &["-P", "OUTPUT", "DROP"])?;
        rule(command, &["-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"])?;
        rule(command, &["-A", "OUTPUT", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"])?;
    }
    if profile == Firewall::OutboundTurnUdp {
        // Destination port only: no new inbound UDP is permitted.
        rule("iptables", &["-A", "OUTPUT", "-p", "udp", "--dport", "3478", "-j", "ACCEPT"])?;
    }
    // Signaling exceptions are TCP/443 only; they never open media ports.
    for address in signaling {
        let destination = format!("{address}/32");
        rule("iptables", &["-A", "OUTPUT", "-d", &destination, "-p", "tcp", "--dport", "443", "-j", "ACCEPT"])?;
    }
    let is_static = !matches!(profile, Firewall::Open | Firewall::OutboundTurnUdp);
    if is_static {
        for cidr in static_cidrs() {
            let allow = |proto: &str, port: &str| {
                rule("iptables", &["-A", "OUTPUT", "-d", cidr, "-p", proto, "--dport", port, "-j", "ACCEPT"])
            };
            allow("tcp", "443")?;
            if matches!(profile, Firewall::StaticFull | Firewall::StaticTcp) {
                allow("tcp", "7881")?;
            }
            if matches!(profile, Firewall::StaticFull | Firewall::StaticUdp) {
                allow("udp", "50000:60000")?;
            }
            if matches!(profile, Firewall::StaticFull | Firewall::StaticUdp | Firewall::StaticTurnUdp) {
                allow("udp", "3478")?;
            }
        }
    }
    log(format!("Container firewall profile: {}", value_name(profile)));
    write_json(
        &out.join("firewall.json"),
        &json!({"profile": value_name(profile), "signaling_ipv4_tcp443": signaling,
                "static_cidrs": if is_static { static_cidrs() } else { vec![] }}),
    )
}

fn save_firewall_counters(profile: Firewall, out: &Path) {
    if profile == Firewall::Open {
        return;
    }
    for (command, name) in [("iptables", "firewall-ipv4.txt"), ("ip6tables", "firewall-ipv6.txt")] {
        if let Ok(output) = Command::new(command).args(["-w", "-L", "-n", "-v", "-x"]).output() {
            let _ = fs::write(out.join(name), [output.stdout, output.stderr].concat());
        }
    }
}

/// TCP/443 reachability per endpoint, without sending credentials.
fn probe_signaling(urls: &BTreeMap<String, String>, out: &Path) -> Result<()> {
    let mut probes = vec![];
    let mut blocked = vec![];
    for (name, url) in urls {
        let mut reachable = false;
        for address in resolve_ipv4([url]) {
            let ip: IpAddr = address.parse()?;
            let ok = TcpStream::connect_timeout(&(ip, 443).into(), Duration::from_secs(3)).is_ok();
            reachable |= ok;
            probes.push(json!({"client": name, "address": address, "tcp443_reachable": ok}));
        }
        if !reachable {
            blocked.push(name.clone());
        }
    }
    write_json(&out.join("endpoint-probes.json"), &json!(probes))?;
    if !blocked.is_empty() {
        bail!("Signaling TCP/443 is blocked for {blocked:?}; add --signaling-ip exceptions");
    }
    Ok(())
}

/// Count unsolicited datagrams; a host-side sender targets this listener.
fn inbound_udp_check(seconds: u64, out: &Path) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:3478")?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    fs::write(out.join("udp-inbound-ready"), "")?;
    log(format!("Listening for unsolicited UDP on 3478 for {seconds}s"));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut received = 0;
    let mut buffer = [0u8; 2048];
    while Instant::now() < deadline {
        if socket.recv_from(&mut buffer).is_ok() {
            received += 1;
        }
    }
    write_json(&out.join("udp-inbound-received.json"), &json!({"unsolicited_packets_received": received}))?;
    if received > 0 {
        bail!("{received} unsolicited inbound UDP packets reached the container");
    }
    Ok(())
}

fn prepare_network(common: &CommonArgs, out: &Path, urls: &BTreeMap<String, String>) -> Result<()> {
    if common.firewall_profile == Firewall::Open {
        if common.inbound_udp_check > 0 {
            bail!("--inbound-udp-check needs a firewall profile");
        }
        return Ok(());
    }
    let mut signaling: BTreeSet<String> = common.signaling_ips.iter().map(|ip| ip.to_string()).collect();
    if common.firewall_profile == Firewall::OutboundTurnUdp {
        // Per-run DNS snapshot of the signaling hosts, resolved before DROP applies.
        signaling.extend(resolve_ipv4(urls.values()));
    }
    apply_firewall(common.firewall_profile, &signaling.into_iter().collect::<Vec<_>>(), out)?;
    probe_signaling(urls, out)?;
    if common.inbound_udp_check > 0 {
        inbound_udp_check(common.inbound_udp_check, out)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Child processes

static STOP: AtomicBool = AtomicBool::new(false);
static CHILD_PIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

extern "C" fn on_signal(_: libc::c_int) {
    if STOP.swap(true, Ordering::SeqCst) {
        // Second signal: abort immediately.
        unsafe { libc::_exit(130) };
    }
}

fn handle_signals() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    // Forward the stop request to children outside the signal handler.
    thread::spawn(|| {
        while !STOP.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        log("Stopping (signal again to abort)...");
        for pid in CHILD_PIDS.lock().unwrap().iter() {
            unsafe { libc::kill(*pid, libc::SIGINT) };
        }
    });
}

struct Process {
    name: String,
    child: Child,
    published: Arc<AtomicBool>,
    readers: Vec<thread::JoinHandle<()>>,
}

impl Process {
    fn spawn(
        name: &str,
        role: &str,
        args: Vec<String>,
        url: &str,
        secrets: &[String],
        out: &Path,
        verbose: bool,
    ) -> Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg(role)
            .args(&args)
            .env("LIVEKIT_URL", url)
            .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_RUST_LOG.into()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group: terminal Ctrl-C reaches only the orchestrator,
            // which then stops children in order so CSVs are flushed.
            .process_group(0);
        let mut child = command.spawn().with_context(|| format!("starting {name}"))?;
        CHILD_PIDS.lock().unwrap().push(child.id() as i32);
        let published = Arc::new(AtomicBool::new(false));
        let log_file = Arc::new(Mutex::new(fs::File::create(out.join(format!("{name}.log")))?));
        let mut readers = vec![];
        let streams: Vec<Box<dyn Read + Send>> = vec![
            Box::new(child.stdout.take().expect("piped stdout")),
            Box::new(child.stderr.take().expect("piped stderr")),
        ];
        for stream in streams {
            let (published, log_file) = (published.clone(), log_file.clone());
            let (secrets, name) = (secrets.to_vec(), name.to_owned());
            readers.push(thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let mut line = line;
                    for secret in &secrets {
                        line = line.replace(secret.as_str(), "[redacted]");
                    }
                    let _ = writeln!(log_file.lock().unwrap(), "{line}");
                    if line.contains("Published camera track") {
                        published.store(true, Ordering::SeqCst);
                    }
                    const ECHO: &[&str] = &[
                        "LiveKit server:", "WebRTC connection:", "Published camera track", "headless summary",
                        "headless progress", "capture_to_decoded", "Data probe summary", "Error", "error:",
                        "panicked",
                    ];
                    if verbose || ECHO.iter().any(|marker| line.contains(marker)) {
                        println!("[{name}] {line}");
                    }
                }
            }));
        }
        Ok(Self { name: name.to_owned(), child, published, readers })
    }

    fn interrupt(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait up to `timeout`, then interrupt, then kill; always drain output.
    fn wait(mut self, timeout: Option<Duration>) -> i32 {
        let deadline = timeout.map(|t| Instant::now() + t);
        while self.running() {
            if deadline.is_some_and(|d| Instant::now() > d) {
                log(format!("{} timed out; interrupting", self.name));
                self.interrupt();
                let grace = Instant::now() + Duration::from_secs(15);
                while self.running() && Instant::now() < grace {
                    thread::sleep(Duration::from_millis(100));
                }
                if self.running() {
                    let _ = self.child.kill();
                }
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let status = self.child.wait().ok();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        let pid = self.child.id() as i32;
        CHILD_PIDS.lock().unwrap().retain(|p| *p != pid);
        status.and_then(|s| s.code()).unwrap_or(1)
    }

    fn stop(self) -> i32 {
        self.interrupt();
        self.wait(Some(Duration::from_secs(15)))
    }
}

fn credentials() -> Result<(String, Vec<String>)> {
    let names = ["LIVEKIT_URL", "LIVEKIT_API_KEY", "LIVEKIT_API_SECRET"];
    let missing: Vec<_> = names.iter().filter(|n| std::env::var(n).map_or(true, |v| v.is_empty())).collect();
    if !missing.is_empty() {
        bail!("Set {missing:?} (e.g. docker run --env-file <file> …, or export them)");
    }
    let values: Vec<String> = names.iter().map(|n| std::env::var(n).unwrap()).collect();
    Ok((values[0].clone(), values))
}

fn publisher_args(video: &VideoArgs, common: &CommonArgs, room: &str, out: &Path) -> Result<Vec<String>> {
    let mut args = vec![
        "--room-name".into(), room.into(), "--identity".into(), video.identity.clone(),
        "--width".into(), video.width.to_string(), "--height".into(), video.height.to_string(),
        "--fps".into(), video.fps.to_string(), "--codec".into(), value_name(video.codec),
        "--encoder".into(), video.encoder.clone(), "--attach-timestamp".into(), "--attach-frame-id".into(),
        "--timing-output".into(), out.join("publisher.csv").display().to_string(),
    ];
    match video.source {
        Source::Pattern => args.extend(["--test-pattern".into(), "--pattern".into(), value_name(video.pattern)]),
        Source::Camera => args.extend(["--camera-index".into(), video.camera_index.to_string()]),
    }
    if let Some(bitrate) = video.max_bitrate {
        args.extend(["--max-bitrate".into(), bitrate.to_string()]);
    }
    if video.max_playout_delay != "server" {
        let max: u32 = video.max_playout_delay.parse().context("--max-playout-delay must be ms or `server`")?;
        args.extend([
            "--min-playout-delay".into(), video.min_playout_delay.to_string(),
            "--max-playout-delay".into(), max.to_string(),
        ]);
    }
    if video.simulcast {
        args.push("--simulcast".into());
    }
    if video.burn_timestamp {
        args.push("--burn-timestamp".into());
    }
    if common.display {
        args.extend(["--display-video".into(), "--display-timing".into()]);
    }
    args.extend(shared_flags(common));
    Ok(args)
}

fn viewer_args(viewer: &ViewerArgs, common: &CommonArgs, identity: &str, room: &str, name: &str, out: &Path) -> Vec<String> {
    let mut args = vec![
        "--room-name".into(), room.into(), "--identity".into(), name.into(), "--participant".into(), identity.into(),
        "--duration".into(), viewer.duration.to_string(), "--warmup".into(), viewer.warmup.to_string(),
        "--timing-output".into(), out.join(format!("{name}.csv")).display().to_string(),
    ];
    if common.display {
        args.push("--display-timestamp".into());
    } else {
        args.extend(["--headless".into(), "--startup-timeout".into(), viewer.startup_timeout.to_string()]);
    }
    if viewer.low_latency {
        args.push("--low-latency".into());
    }
    args.extend(shared_flags(common));
    args
}

fn shared_flags(common: &CommonArgs) -> Vec<String> {
    let mut flags = vec![];
    if common.relay_only || common.firewall_profile == Firewall::StaticTurnUdp {
        flags.push("--relay-only".into());
    }
    if common.data_probe {
        flags.push("--data-probe".into());
    }
    flags
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Meta {
    role: String,
    label: String,
    endpoint: Option<String>,
    expected_region: Option<String>,
    exit_code: Option<i32>,
    duration: Option<u64>,
    data_probe: bool,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn slug(text: &str) -> String {
    text.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').collect()
}

fn check_display(common: &CommonArgs) -> Result<()> {
    if common.display && Path::new("/.dockerenv").exists() {
        bail!("--display needs a desktop session; run the native binary instead of Docker");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Modes

fn mode_run(args: RunArgs) -> Result<i32> {
    check_display(&args.common)?;
    let (base_url, mut secrets) = credentials()?;
    let run_id = args.run_id.clone().unwrap_or_else(|| format!("lk-latency-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
    let out = args.common.output.join(&run_id);
    fs::create_dir_all(&out)?;
    let viewer_specs = if args.viewer_regions.is_empty() { vec!["default".to_owned()] } else { args.viewer_regions.clone() };
    let mut urls = BTreeMap::new();
    urls.insert("publisher".to_owned(), resolve_endpoint(&args.publisher_region, &base_url)?);
    let mut viewers = vec![];
    for (index, spec) in viewer_specs.iter().enumerate() {
        let name = format!("viewer-{}-{}", index + 1, slug(&describe_endpoint(spec)));
        urls.insert(name.clone(), resolve_endpoint(spec, &base_url)?);
        viewers.push((name, spec.clone()));
    }
    secrets.extend(urls.values().cloned());
    let config = config_json(&args.video, Some(&args.viewer), &args.common);
    write_json(&out.join("run.json"), &json!({
        "run_id": run_id, "iterations": args.iterations, "publisher_endpoint": describe_endpoint(&args.publisher_region),
        "viewer_endpoints": viewer_specs.iter().map(|s| describe_endpoint(s)).collect::<Vec<_>>(),
        "config": config, "build": build_info(), "started_at": chrono::Utc::now().to_rfc3339(),
    }))?;
    log(format!(
        "Run {run_id}: publisher={} viewers={:?} iterations={} results={}",
        describe_endpoint(&args.publisher_region),
        viewer_specs.iter().map(|s| describe_endpoint(s)).collect::<Vec<_>>(),
        args.iterations,
        out.display()
    ));
    prepare_network(&args.common, &out, &urls)?;

    let mut sessions = vec![];
    for iteration in 1..=args.iterations {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        let session = out.join(format!("iter-{iteration:02}"));
        fs::create_dir_all(&session)?;
        if out.join("firewall.json").exists() {
            fs::copy(out.join("firewall.json"), session.join("firewall.json"))?;
        }
        let room = format!("{run_id}-{iteration}");
        log(format!("Iteration {iteration}/{}: room {room}", args.iterations));
        let publisher = Process::spawn(
            "publisher", "publisher", publisher_args(&args.video, &args.common, &room, &session)?,
            &urls["publisher"], &secrets, &session, args.common.verbose,
        )?;
        let deadline = Instant::now() + Duration::from_secs(args.viewer.startup_timeout);
        let mut publisher = publisher;
        while !publisher.published.load(Ordering::SeqCst)
            && publisher.running()
            && Instant::now() < deadline
            && !STOP.load(Ordering::SeqCst)
        {
            thread::sleep(Duration::from_millis(100));
        }
        let published = publisher.published.load(Ordering::SeqCst);
        if !published {
            log(if publisher.running() {
                "Publisher did not publish before the startup timeout"
            } else {
                "Publisher exited before publishing (see its error above)"
            });
        }
        let mut started = vec![];
        if published {
            for (name, spec) in &viewers {
                let process = Process::spawn(
                    name, "subscriber",
                    viewer_args(&args.viewer, &args.common, &args.video.identity, &room, name, &session),
                    &urls[name], &secrets, &session, args.common.verbose,
                )?;
                started.push((name.clone(), spec.clone(), process));
            }
        }
        let budget = Duration::from_secs(args.viewer.duration + args.viewer.warmup + args.viewer.startup_timeout + 60);
        for (name, spec, process) in started {
            let code = process.wait(Some(budget));
            write_json(&session.join(format!("{name}.json")), &Meta {
                role: "viewer".into(), label: describe_endpoint(&spec), endpoint: Some(describe_endpoint(&spec)),
                expected_region: expected_region(&spec), exit_code: Some(code), duration: Some(args.viewer.duration),
                data_probe: args.common.data_probe,
            })?;
        }
        let code = publisher.stop();
        write_json(&session.join("publisher.json"), &Meta {
            role: "publisher".into(), label: "publisher".into(), endpoint: Some(describe_endpoint(&args.publisher_region)),
            expected_region: expected_region(&args.publisher_region),
            exit_code: Some(if published { code } else { code.max(1) }), duration: Some(args.viewer.duration),
            data_probe: args.common.data_probe,
        })?;
        sessions.push(session);
        if iteration < args.iterations && !STOP.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs_f64(args.pause));
        }
    }
    save_firewall_counters(args.common.firewall_profile, &out);

    let results: Vec<SessionResult> = sessions.iter().map(|d| analyze_dir(d)).collect::<Result<_>>()?;
    let aggregate = aggregate(&sessions)?;
    write_json(&out.join("summary.json"), &json!({
        "run_id": run_id, "config": config, "build": build_info(), "sessions": results, "aggregate": aggregate,
    }))?;
    let playout = if args.video.max_playout_delay == "server" {
        "server default".to_owned()
    } else {
        format!("{}-{}ms", args.video.min_playout_delay, args.video.max_playout_delay)
    };
    let source = match args.video.source {
        Source::Pattern => format!("{} pattern", value_name(args.video.pattern)),
        Source::Camera => format!("camera {}", args.video.camera_index),
    };
    let header = format!(
        "LiveKit latency run {run_id}: {} x {}s, {} ({}) {}x{}@{} {source}, room playout delay {playout}{}, firewall {}\n\
         build: lk-latency {} ({}) sdk {} {}-{}{}",
        sessions.len(), args.viewer.duration, value_name(args.video.codec), args.video.encoder,
        args.video.width, args.video.height, args.video.fps,
        if args.viewer.low_latency { ", client zero playout" } else { "" },
        value_name(args.common.firewall_profile), env!("CARGO_PKG_VERSION"), crate::BUILD_COMMIT,
        &crate::SDK_REV[..crate::SDK_REV.len().min(12)], std::env::consts::OS, std::env::consts::ARCH,
        if Path::new("/.dockerenv").exists() { " (docker)" } else { "" },
    );
    write_summary(&out, &header, &results, Some(&aggregate))?;
    log(format!("Results: {} (summary.txt, summary.json, summary.csv)", out.display()));
    Ok(if results.is_empty() || results.iter().any(|r| !r.errors.is_empty()) { 1 } else { 0 })
}

fn mode_publish(args: PublishArgs) -> Result<i32> {
    check_display(&args.common)?;
    let (base_url, mut secrets) = credentials()?;
    let out = args.common.output.join(&args.room);
    fs::create_dir_all(&out)?;
    let url = resolve_endpoint(&args.region, &base_url)?;
    secrets.push(url.clone());
    prepare_network(&args.common, &out, &BTreeMap::from([("publisher".to_owned(), url.clone())]))?;
    log(format!("Publishing to room {} via {}; results={}", args.room, describe_endpoint(&args.region), out.display()));
    let publisher = Process::spawn(
        "publisher", "publisher", publisher_args(&args.video, &args.common, &args.room, &out)?,
        &url, &secrets, &out, args.common.verbose,
    )?;
    let code = match args.duration {
        Some(seconds) => {
            let deadline = Instant::now() + Duration::from_secs(seconds);
            let mut publisher = publisher;
            while publisher.running() && Instant::now() < deadline && !STOP.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
            publisher.stop()
        }
        None => publisher.wait(None),
    };
    write_json(&out.join("publisher.json"), &Meta {
        role: "publisher".into(), label: "publisher".into(), endpoint: Some(describe_endpoint(&args.region)),
        expected_region: expected_region(&args.region), exit_code: Some(code), duration: None,
        data_probe: args.common.data_probe,
    })?;
    save_firewall_counters(args.common.firewall_profile, &out);
    log(format!("Publisher stopped. After viewers finish: lk-latency summarize {}", out.display()));
    Ok(code)
}

fn mode_view(args: ViewArgs) -> Result<i32> {
    check_display(&args.common)?;
    let (base_url, mut secrets) = credentials()?;
    let out = args.common.output.join(&args.room);
    fs::create_dir_all(&out)?;
    let url = resolve_endpoint(&args.region, &base_url)?;
    secrets.push(url.clone());
    let name = args.name.clone().unwrap_or_else(|| format!("viewer-{}", slug(&describe_endpoint(&args.region))));
    if out.join(format!("{name}.csv")).exists() {
        bail!("{} exists; pass a different --name", out.join(format!("{name}.csv")).display());
    }
    prepare_network(&args.common, &out, &BTreeMap::from([(name.clone(), url.clone())]))?;
    log(format!("Viewing room {} via {}; results={}", args.room, describe_endpoint(&args.region), out.display()));
    let viewer = Process::spawn(
        &name, "subscriber", viewer_args(&args.viewer, &args.common, &args.identity, &args.room, &name, &out),
        &url, &secrets, &out, args.common.verbose,
    )?;
    let code = viewer.wait(Some(Duration::from_secs(
        args.viewer.duration + args.viewer.warmup + args.viewer.startup_timeout + 60,
    )));
    write_json(&out.join(format!("{name}.json")), &Meta {
        role: "viewer".into(), label: name.clone(), endpoint: Some(describe_endpoint(&args.region)),
        expected_region: expected_region(&args.region), exit_code: Some(code), duration: Some(args.viewer.duration),
        data_probe: args.common.data_probe,
    })?;
    save_firewall_counters(args.common.firewall_profile, &out);
    let mut result = analyze_dir(&out)?;
    result.viewers.retain(|n, _| *n == name);
    result.publisher = None;
    result.errors.retain(|e| e.starts_with(&name));
    let note = if out.join("publisher.csv").exists() {
        ""
    } else {
        " (viewer-only; run `summarize` after the publisher stops for the full breakdown)"
    };
    write_summary(&out, &format!("LiveKit latency view {}/{name}{note}", args.room), &[result.clone()], None)?;
    Ok(if result.errors.is_empty() { 0 } else { 1 })
}

fn mode_summarize(directory: &Path) -> Result<i32> {
    let mut sessions: Vec<PathBuf> = fs::read_dir(directory)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("iter-")))
        .collect();
    sessions.sort();
    if sessions.is_empty() {
        sessions.push(directory.to_owned());
    }
    let results: Vec<SessionResult> = sessions.iter().map(|d| analyze_dir(d)).collect::<Result<_>>()?;
    let aggregate = aggregate(&sessions)?;
    write_json(&directory.join("summary.json"), &json!({"sessions": results, "aggregate": aggregate}))?;
    let name = directory.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    write_summary(directory, &format!("LiveKit latency summary {name}"), &results, Some(&aggregate))?;
    Ok(if results.iter().any(|r| !r.errors.is_empty()) { 1 } else { 0 })
}

fn config_json(video: &VideoArgs, viewer: Option<&ViewerArgs>, common: &CommonArgs) -> Value {
    json!({
        "source": value_name(video.source), "pattern": value_name(video.pattern), "camera_index": video.camera_index,
        "codec": value_name(video.codec), "encoder": video.encoder, "width": video.width, "height": video.height,
        "fps": video.fps, "max_bitrate": video.max_bitrate, "simulcast": video.simulcast,
        "min_playout_delay": video.min_playout_delay, "max_playout_delay": video.max_playout_delay,
        "duration": viewer.map(|v| v.duration), "warmup": viewer.map(|v| v.warmup),
        "low_latency": viewer.map(|v| v.low_latency), "display": common.display,
        "firewall_profile": value_name(common.firewall_profile), "relay_only": common.relay_only,
        "data_probe": common.data_probe,
    })
}

// ---------------------------------------------------------------------------
// Analysis

type Key = (String, u64, u32);
type Events = HashMap<Key, HashMap<String, (u64, bool)>>;

/// Map (track, capture_us, frame_id) -> stage -> (timestamp_us, measured).
fn read_events(path: &Path) -> Result<Events> {
    let mut frames: Events = HashMap::new();
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split(',').collect();
        let [sid, stage, timestamp, capture, frame_id, measured] = fields[..] else { continue };
        let (Ok(timestamp), Ok(capture), Ok(frame_id)) = (timestamp.parse(), capture.parse::<u64>(), frame_id.parse())
        else {
            continue;
        };
        if capture == 0 {
            continue;
        }
        let stages = frames.entry((sid.to_owned(), capture, frame_id)).or_default();
        // With simulcast a stage repeats per layer; keep the earliest.
        let value = (timestamp, measured == "true");
        stages.entry(stage.to_owned()).and_modify(|v| if value.0 < v.0 { *v = value }).or_insert(value);
    }
    Ok(frames)
}

#[derive(Serialize, Clone)]
struct Stats {
    samples: usize,
    mean_ms: f64,
    p50_ms: f64,
    p90_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn stats(values: &[u64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let pick = |p: f64| v[((v.len() as f64 * p / 100.0).ceil() as usize).max(1) - 1] as f64 / 1000.0;
    Some(Stats {
        samples: v.len(),
        mean_ms: round3(v.iter().sum::<u64>() as f64 / v.len() as f64 / 1000.0),
        p50_ms: round3(pick(50.0)),
        p90_ms: round3(pick(90.0)),
        p95_ms: round3(pick(95.0)),
        p99_ms: round3(pick(99.0)),
        min_ms: round3(v[0] as f64 / 1000.0),
        max_ms: round3(v[v.len() - 1] as f64 / 1000.0),
    })
}

#[derive(Serialize, Clone, Default)]
struct ProcessInfo {
    endpoint: Option<String>,
    server_regions: Vec<String>,
    paths: Vec<String>,
    encoder: Option<String>,
    codec_fallback: bool,
    fps: Option<f64>,
    frame_id_gaps: Option<u64>,
    data_probe: Option<Value>,
}

#[derive(Serialize, Clone, Default)]
struct ViewerResult {
    label: String,
    #[serde(flatten)]
    info: ProcessInfo,
    frames_delivered: usize,
    negative_samples: usize,
    frames_published_in_window: Option<usize>,
    frames_dropped_pct: Option<f64>,
    segments: BTreeMap<String, Stats>,
}

#[derive(Serialize, Clone)]
struct SessionResult {
    directory: String,
    errors: Vec<String>,
    publisher: Option<ProcessInfo>,
    viewers: BTreeMap<String, ViewerResult>,
    paired: Option<Value>,
}

fn segment_samples(publisher: Option<&Events>, viewer: &Events) -> (BTreeMap<&'static str, Vec<u64>>, Vec<Key>, usize) {
    let mut samples: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    let mut measured = vec![];
    let mut negative = 0;
    for (key, stages) in viewer {
        if !stages.get("FrameSink").is_some_and(|s| s.1) {
            continue;
        }
        measured.push(key.clone());
        let mut times: HashMap<&str, u64> = stages.iter().map(|(s, v)| (s.as_str(), v.0)).collect();
        times.insert("capture", key.1);
        if let Some(stages) = publisher.and_then(|p| p.get(key)) {
            for stage in PUBLISHER_STAGES {
                if let Some(v) = stages.get(*stage) {
                    times.insert(stage, v.0);
                }
            }
        }
        for (name, _, from, to) in SEGMENTS {
            if let (Some(a), Some(b)) = (times.get(from), times.get(to)) {
                match b.checked_sub(*a) {
                    Some(delta) => samples.entry(name).or_default().push(delta),
                    None => negative += 1,
                }
            }
        }
    }
    (samples, measured, negative)
}

fn analyze_viewer(publisher: Option<&Events>, viewer: &Events, result: &mut ViewerResult) {
    let (samples, measured, negative) = segment_samples(publisher, viewer);
    result.frames_delivered = measured.len();
    // Delivered frame rate from the sink timestamps; works for windowed viewers too.
    let mut sinks: Vec<u64> = measured.iter().filter_map(|k| viewer[k].get("FrameSink").map(|v| v.0)).collect();
    sinks.sort_unstable();
    if let (Some(first), Some(last)) = (sinks.first(), sinks.last()) {
        if last > first && result.info.fps.is_none() {
            result.info.fps = Some(round3((sinks.len() - 1) as f64 / ((last - first) as f64 / 1e6)));
        }
    }
    result.negative_samples = negative;
    result.segments = samples.iter().filter_map(|(k, v)| stats(v).map(|s| (k.to_string(), s))).collect();
    if let (Some(publisher), false) = (publisher, measured.is_empty()) {
        // Frames published during the viewer's measurement window that never arrived.
        let lo = measured.iter().map(|k| k.1).min().unwrap();
        let hi = measured.iter().map(|k| k.1).max().unwrap();
        let sids: BTreeSet<&String> = measured.iter().map(|k| &k.0).collect();
        let expected: Vec<&Key> = publisher.keys().filter(|k| sids.contains(&k.0) && lo <= k.1 && k.1 <= hi).collect();
        let delivered = expected.iter().filter(|k| viewer.get(**k).is_some_and(|s| s.contains_key("FrameSink"))).count();
        result.frames_published_in_window = Some(expected.len());
        result.frames_dropped_pct =
            (!expected.is_empty()).then(|| round3(100.0 * (1.0 - delivered as f64 / expected.len() as f64)));
    }
}

fn paired_comparison(viewers: &BTreeMap<String, Events>) -> Option<Value> {
    let names: Vec<&String> = viewers.keys().collect();
    if names.len() < 2 {
        return None;
    }
    let decodable = |events: &Events| -> BTreeSet<Key> {
        events
            .iter()
            .filter(|(_, s)| s.get("FrameSink").is_some_and(|v| v.1) && s.contains_key("WebrtcReceive"))
            .map(|(k, _)| k.clone())
            .collect()
    };
    let mut common = decodable(&viewers[names[0]]);
    for name in &names[1..] {
        common = common.intersection(&decodable(&viewers[*name])).cloned().collect();
    }
    if common.is_empty() {
        return None;
    }
    let base = names[0];
    let mut result = json!({"reference": base, "common_frames": common.len()});
    for other in &names[1..] {
        let mut delta: Vec<i64> = common
            .iter()
            .map(|k| viewers[*other][k]["WebrtcReceive"].0 as i64 - viewers[base][k]["WebrtcReceive"].0 as i64)
            .collect();
        delta.sort_unstable();
        let first = 100.0 * delta.iter().filter(|d| **d < 0).count() as f64 / delta.len() as f64;
        result[format!("{other}_minus_{base}_receive_ms")] = json!({
            "p50_ms": round3(delta[delta.len() / 2] as f64 / 1000.0),
            "min_ms": round3(delta[0] as f64 / 1000.0), "max_ms": round3(delta[delta.len() - 1] as f64 / 1000.0),
            "other_first_pct": (first * 100.0).round() / 100.0,
        });
    }
    Some(result)
}

fn capture<'a>(text: &'a str, prefix: &str, end: char) -> Vec<&'a str> {
    text.match_indices(prefix)
        .filter_map(|(i, _)| text[i + prefix.len()..].split(end).next())
        .collect()
}

fn parse_log(text: &str) -> ProcessInfo {
    let mut info = ProcessInfo {
        server_regions: capture(text, "LiveKit server: region=", '\n')
            .iter()
            .filter_map(|s| s.split(" node_id=").next())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        codec_fallback: text.contains("H.264 fallback"),
        ..Default::default()
    };
    info.paths = text
        .lines()
        .filter(|l| l.contains("WebRTC connection: peer="))
        .filter_map(|l| {
            let peer = l.split("peer=").nth(1)?.split_whitespace().next()?;
            let path = l.split(" path=").nth(1)?.split_whitespace().next()?;
            Some(format!("{peer}:{path}"))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    info.encoder = text
        .lines()
        .rev()
        .filter_map(|l| l.split("Publish health: ").nth(1)?.split("encoder=").nth(1))
        .map(str::trim)
        .find(|e| !e.is_empty())
        .map(str::to_owned);
    if let Some(summary) = text.lines().rev().find_map(|l| l.split("headless summary: ").nth(1)) {
        let field = |name: &str| summary.split(&format!("{name}=")).nth(1)?.split_whitespace().next().map(str::to_owned);
        info.fps = field("fps").and_then(|v| v.parse().ok());
        info.frame_id_gaps = field("frame_id_gaps").and_then(|v| v.parse().ok());
    }
    if let Some(probe) = text.lines().rev().find_map(|l| l.split("Data probe summary: ").nth(1)) {
        let field = |name: &str| probe.split(&format!("{name}=")).nth(1)?.split_whitespace().next().map(str::to_owned);
        let number = |name: &str| field(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        info.data_probe = Some(json!({"role": field("role"), "reliable": number("reliable"),
                                      "lossy": number("lossy"), "errors": number("errors")}));
    }
    info
}

fn validate(meta: &Meta, info: &ProcessInfo, profile: &str) -> Vec<String> {
    let mut errors = vec![];
    if let Some(code) = meta.exit_code.filter(|c| *c != 0) {
        errors.push(format!("exited with {code}"));
    }
    if let Some(expected) = &meta.expected_region {
        if info.server_regions.is_empty() || !info.server_regions.iter().all(|r| region_matches(r, expected)) {
            errors.push(format!("expected region {expected}, server reported {:?}", info.server_regions));
        }
    }
    if meta.exit_code == Some(0) && info.paths.is_empty() {
        errors.push("no selected ICE connection reported".into());
    }
    let paths: Vec<&str> = info.paths.iter().filter_map(|p| p.split_once(':').map(|(_, path)| path)).collect();
    match profile {
        "static-tls" if paths.iter().any(|p| *p != "TURN/TLS") => {
            errors.push(format!("expected only TURN/TLS paths, saw {paths:?}"))
        }
        "static-tcp" if paths.iter().any(|p| matches!(*p, "ICE/UDP" | "TURN/UDP")) => {
            errors.push(format!("unexpected UDP path {paths:?}"))
        }
        "outbound-turn-udp" if paths.iter().any(|p| *p != "TURN/UDP") => {
            errors.push(format!("expected only TURN/UDP paths, saw {paths:?}"))
        }
        _ => {}
    }
    if meta.data_probe {
        let minimum = (meta.duration.unwrap_or(2) / 2).max(1);
        let ok = info.data_probe.as_ref().is_some_and(|p| {
            p["errors"] == 0 && p["reliable"].as_u64() >= Some(minimum) && p["lossy"].as_u64() >= Some(minimum)
        });
        if !ok {
            errors.push(format!("data probe failed: {:?}", info.data_probe));
        }
    }
    if info.codec_fallback {
        errors.push("publisher fell back to H.264".into());
    }
    errors
}

struct LoadedSession {
    result: SessionResult,
    viewer_events: BTreeMap<String, Events>,
    publisher: Option<Events>,
}

fn load_session(directory: &Path) -> Result<LoadedSession> {
    let read_text = |name: &str| fs::read_to_string(directory.join(name)).unwrap_or_default();
    let mut metas: BTreeMap<String, Meta> = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") if directory.join(format!("{stem}.log")).exists() => {
                if let Ok(meta) = serde_json::from_str(&fs::read_to_string(&path)?) {
                    metas.insert(stem, meta);
                }
            }
            _ => {}
        }
    }
    // CSVs written by the binaries directly have no metadata; treat them as viewers.
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "csv") {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            if !metas.contains_key(&stem) && stem != "publisher" && stem != "summary" {
                metas.insert(stem.clone(), Meta { role: "viewer".into(), label: stem, ..Default::default() });
            }
        }
    }
    let profile = serde_json::from_str::<Value>(&read_text("firewall.json"))
        .ok()
        .and_then(|v| v["profile"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "open".into());
    let publisher_csv = directory.join("publisher.csv");
    let publisher = if publisher_csv.metadata().is_ok_and(|m| m.len() > 0) { Some(read_events(&publisher_csv)?) } else { None };
    let mut result = SessionResult {
        directory: directory.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        errors: vec![],
        publisher: None,
        viewers: BTreeMap::new(),
        paired: None,
    };
    if let Some(meta) = metas.get("publisher") {
        let mut info = parse_log(&read_text("publisher.log"));
        info.endpoint = meta.endpoint.clone();
        result.errors.extend(validate(meta, &info, &profile).into_iter().map(|e| format!("publisher: {e}")));
        result.publisher = Some(info);
    }
    let mut viewer_events = BTreeMap::new();
    for (name, meta) in metas.iter().filter(|(_, m)| m.role == "viewer") {
        let mut info = parse_log(&read_text(&format!("{name}.log")));
        info.endpoint = meta.endpoint.clone();
        result.errors.extend(validate(meta, &info, &profile).into_iter().map(|e| format!("{name}: {e}")));
        let mut viewer = ViewerResult { label: meta.label.clone(), info, ..Default::default() };
        let csv = directory.join(format!("{name}.csv"));
        if csv.exists() {
            let events = read_events(&csv)?;
            analyze_viewer(publisher.as_ref(), &events, &mut viewer);
            viewer_events.insert(name.clone(), events);
        }
        if viewer.frames_delivered == 0 {
            result.errors.push(format!("{name}: no frames measured"));
        }
        result.viewers.insert(name.clone(), viewer);
    }
    result.paired = paired_comparison(&viewer_events);
    Ok(LoadedSession { result, viewer_events, publisher })
}

fn analyze_dir(directory: &Path) -> Result<SessionResult> {
    Ok(load_session(directory)?.result)
}

/// Pool raw samples across iterations per viewer label.
fn aggregate(sessions: &[PathBuf]) -> Result<Value> {
    let mut pooled: BTreeMap<String, BTreeMap<&'static str, Vec<u64>>> = BTreeMap::new();
    let mut per_iteration: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for directory in sessions {
        let session = load_session(directory)?;
        for (name, events) in &session.viewer_events {
            let viewer = &session.result.viewers[name];
            let (samples, _, _) = segment_samples(session.publisher.as_ref(), events);
            let bucket = pooled.entry(viewer.label.clone()).or_default();
            for (segment, values) in samples {
                bucket.entry(segment).or_default().extend(values);
            }
            if let Some(total) = viewer.segments.get("total") {
                per_iteration.entry(viewer.label.clone()).or_default().push(total.p50_ms);
            }
        }
    }
    let mut out = serde_json::Map::new();
    for (label, segments) in pooled {
        let stats: BTreeMap<&str, Stats> = segments.iter().filter_map(|(k, v)| stats(v).map(|s| (*k, s))).collect();
        out.insert(label.clone(), json!({
            "segments": stats, "total_p50_per_iteration_ms": per_iteration.get(&label).cloned().unwrap_or_default(),
        }));
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// Reporting

fn segment_table(segments: &BTreeMap<String, Stats>, lines: &mut Vec<String>) {
    lines.push(format!("    {:34} {:>9} {:>9} {:>9} {:>9}  samples", "stage", "p50", "p95", "p99", "max"));
    for (key, label, _, _) in SEGMENTS {
        if let Some(s) = segments.get(*key) {
            lines.push(format!(
                "    {label:34} {:9.2} {:9.2} {:9.2} {:9.2}  {}",
                s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms, s.samples
            ));
        }
    }
}

fn short_paths(paths: &[String]) -> String {
    let set: BTreeSet<&str> = paths.iter().map(|p| p.split_once(':').map_or(p.as_str(), |(_, path)| path)).collect();
    if set.is_empty() {
        "?".into()
    } else {
        set.into_iter().collect::<Vec<_>>().join(",")
    }
}

fn regions(info: &ProcessInfo) -> String {
    if info.server_regions.is_empty() {
        "?".into()
    } else {
        info.server_regions.join(",")
    }
}

fn write_summary(out: &Path, header: &str, sessions: &[SessionResult], aggregate: Option<&Value>) -> Result<()> {
    let mut lines = vec![header.to_owned(), String::new()];
    for result in sessions {
        lines.push(format!("[{}] {}", result.directory, if result.errors.is_empty() { "OK" } else { "FAILED" }));
        if let Some(publisher) = &result.publisher {
            lines.push(format!(
                "  publisher [{}] region={} path={} encoder={}",
                publisher.endpoint.as_deref().unwrap_or("?"), regions(publisher), short_paths(&publisher.paths),
                publisher.encoder.as_deref().unwrap_or("?")
            ));
        }
        for (name, viewer) in &result.viewers {
            let mut line = format!(
                "  {name} [{}] region={} path={} fps={} frames={}",
                viewer.info.endpoint.as_deref().unwrap_or("?"), regions(&viewer.info), short_paths(&viewer.info.paths),
                viewer.info.fps.map_or("?".into(), |f| f.to_string()), viewer.frames_delivered
            );
            if let Some(dropped) = viewer.frames_dropped_pct {
                let _ = write!(line, " dropped={dropped}%");
            }
            lines.push(line);
            segment_table(&viewer.segments, &mut lines);
        }
        if let Some(paired) = &result.paired {
            lines.push(format!("  paired: {paired}"));
        }
        lines.extend(result.errors.iter().map(|e| format!("  ERROR {e}")));
        lines.push(String::new());
    }
    if let (Some(Value::Object(aggregate)), true) = (aggregate, sessions.len() > 1) {
        lines.push(format!("Pooled across {} iterations:", sessions.len()));
        for (label, data) in aggregate {
            lines.push(format!("  {label}: total p50 per iteration (ms) = {}", data["total_p50_per_iteration_ms"]));
            let segments: BTreeMap<String, Stats> = data["segments"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| {
                            let f = |n: &str| v[n].as_f64().unwrap_or(0.0);
                            Some((k.clone(), Stats {
                                samples: v["samples"].as_u64()? as usize, mean_ms: f("mean_ms"), p50_ms: f("p50_ms"),
                                p90_ms: f("p90_ms"), p95_ms: f("p95_ms"), p99_ms: f("p99_ms"), min_ms: f("min_ms"),
                                max_ms: f("max_ms"),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            segment_table(&segments, &mut lines);
        }
        lines.push(String::new());
    }
    lines.push(
        "All times in ms. 'TOTAL capture -> decoded' excludes rendering/display. Stage percentiles are computed \
         independently and do not add up to the total."
            .into(),
    );
    let text = lines.join("\n") + "\n";
    fs::write(out.join("summary.txt"), &text)?;

    let mut csv = String::from(
        "session,viewer,publisher_endpoint,viewer_endpoint,publisher_region,viewer_region,publisher_path,viewer_path,fps,frames,dropped_pct,ok",
    );
    for (key, _, _, _) in SEGMENTS {
        for p in ["p50", "p95", "p99"] {
            let _ = write!(csv, ",{key}_{p}_ms");
        }
    }
    csv.push('\n');
    let opt = |v: Option<String>| v.unwrap_or_default();
    for result in sessions {
        let publisher = result.publisher.clone().unwrap_or_default();
        for viewer in result.viewers.values() {
            let _ = write!(
                csv,
                "{},{},{},{},{},{},{},{},{},{},{},{}",
                result.directory, viewer.label, opt(publisher.endpoint.clone()), opt(viewer.info.endpoint.clone()),
                publisher.server_regions.join(";"), viewer.info.server_regions.join(";"), publisher.paths.join(";"),
                viewer.info.paths.join(";"), opt(viewer.info.fps.map(|f| f.to_string())), viewer.frames_delivered,
                opt(viewer.frames_dropped_pct.map(|d| d.to_string())), result.errors.is_empty()
            );
            for (key, _, _, _) in SEGMENTS {
                let s = viewer.segments.get(*key);
                for value in [s.map(|s| s.p50_ms), s.map(|s| s.p95_ms), s.map(|s| s.p99_ms)] {
                    let _ = write!(csv, ",{}", opt(value.map(|v| v.to_string())));
                }
            }
            csv.push('\n');
        }
    }
    fs::write(out.join("summary.csv"), csv)?;
    println!("\n{text}");
    Ok(())
}
