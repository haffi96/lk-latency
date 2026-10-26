# lk-latency

Measure LiveKit video latency from publisher to viewer, per stage, across
regional endpoints. One self-contained binary contains the publisher, the
viewer, and the orchestrator. It runs on macOS (Apple Silicon) and Linux, either
natively or in Docker.

- **Docker:** the image contains the exact Linux release binary, unchanged.
  `lk-latency version` prints the binary's SHA-256 so you can check.
- **Native:** adds camera capture, hardware encoding, and on-screen windows.
- **Shared source:** native and Docker builds come from the same source and lockfile, and use
  the same pinned [LiveKit Rust SDK](https://github.com/livekit/rust-sdks) commit
  (see `Cargo.toml`).

## Credentials

Pass them on the command line (works the same in Docker):

```sh
lk-latency run --url wss://<project>.livekit.cloud --api-key <key> --api-secret <secret> --viewer-region eu
```

Or, to avoid retyping them (and keep the secret out of shell history), put them in a
private file and load it. The flags override the file.

```sh
mkdir -p ~/.config/lk-latency && cat > ~/.config/lk-latency/cloud.env <<'EOF'
LIVEKIT_URL=wss://<project>.livekit.cloud
LIVEKIT_API_KEY=...
LIVEKIT_API_SECRET=...
EOF
chmod 600 ~/.config/lk-latency/cloud.env
```

Never put credentials in an image or a repo. They are redacted from saved logs.

## Install

**Docker** (macOS or Linux):

```sh
IMG=ghcr.io/haffi96/lk-latency:latest      # or a release tag, e.g. :v0.2.0
docker run --rm -v "$PWD/results:/results" $IMG run --url wss://<project>.livekit.cloud --api-key <key> --api-secret <secret> --viewer-region eu
# or: docker run --rm --env-file ~/.config/lk-latency/cloud.env -v "$PWD/results:/results" $IMG run --viewer-region eu
```

`latest` is the newest release, `edge` is the newest `main` build, and each
release is also tagged with its version. The image is multi-arch (amd64, arm64),
so Docker Desktop on Apple Silicon runs the arm64 Linux binary without emulation.

**Native** binary from the releases page (platforms: `macos-arm64`,
`linux-amd64`, `linux-arm64`):

```sh
V=v0.1.0 P=macos-arm64
BASE=https://github.com/haffi96/lk-latency/releases/download/$V
curl -fsSL -O $BASE/lk-latency-$V-$P.tar.gz -O $BASE/SHA256SUMS
shasum -a 256 -c --ignore-missing SHA256SUMS   # or: sha256sum -c --ignore-missing SHA256SUMS
tar -xzf lk-latency-$V-$P.tar.gz && ./lk-latency version
set -a; . ~/.config/lk-latency/cloud.env; set +a
```

`lk-latency version` prints the binary's SHA-256, which matches the
`<platform>/lk-latency` line in `SHA256SUMS`.

The macOS binary is unsigned. Downloading it with `curl` avoids Gatekeeper's
quarantine. If you downloaded it in a browser, clear the flag with
`xattr -d com.apple.quarantine lk-latency`. The Linux binary needs a glibc 2.35+
distro (Ubuntu 22.04+, Debian 12+).

**macOS camera access:** `--source camera` needs camera permission for the app
that runs `lk-latency` (Terminal, iTerm, your IDE, …). The first camera run
triggers the macOS prompt. If access was denied, allow the app in System Settings
→ Privacy & Security → Camera, then restart the app. If no camera frame arrives
within 10 s, the publisher exits with an error rather than publishing black
frames.

**Local server** (no cloud account; useful to check a setup):

```sh
brew install livekit   # or see https://docs.livekit.io/home/self-hosting/local/
livekit-server --dev &
./lk-latency run --url ws://127.0.0.1:7880 --api-key devkey --api-secret secret --duration 10
```

From Docker, use `--url ws://host.docker.internal:7880`.

On Linux, if the viewer reports `No video frames received`, start the server with
your LAN address: `livekit-server --dev --node-ip <LAN IP>`. Media then uses that
address. libwebrtc ignores loopback candidates, so `--node-ip 127.0.0.1` never connects.

## Measure

```sh
# Publisher in EU, simultaneous viewers in EU and US, 5 fresh-room iterations
lk-latency run --publisher-region eu --viewer-region eu --viewer-region us --duration 30 --iterations 5

# Native only: real camera, hardware encoder, and windows with live stats
lk-latency run --source camera --encoder videotoolbox --codec h264 --display --viewer-region eu
```

Results go to `results/<run-id>/` (`/results` in Docker):

- `summary.txt`: a per-stage table for each iteration and viewer, then pooled stats.
- `summary.csv`: one row per iteration and viewer, for comparing experiments.
- `summary.json`: all of the above plus build identity and paired viewer deltas.
- `iter-NN/`: raw per-frame timing CSVs and redacted logs.

### Publisher and viewer started separately

Use a shared room and output directory. Start the publisher first, because it recreates the room
to apply the playout delay.

```sh
lk-latency publish --room exp1 --region eu
lk-latency view --room exp1 --region us --duration 30      # another terminal / container
lk-latency summarize results/exp1                          # after stopping the publisher
```

To restrict only the publisher's network, run it in Docker under a firewall profile and run the
viewers natively on the same host, so they share a clock. The container writes as root, so
create the room directory first and make it writable for the viewers:

```sh
mkdir -p results/exp1 && chmod 777 results/exp1
docker run --rm --cap-add NET_ADMIN --env-file ~/.config/lk-latency/cloud.env -v "$PWD/results:/results" \
  $IMG publish --room exp1 --region uae --firewall-profile static-tls --duration 60
lk-latency view --room exp1 --region uae --duration 30     # unrestricted; another terminal
lk-latency summarize results/exp1
```

Each process's own profile is recorded in its `.json` file in the room directory, so
`summarize` checks the publisher's path against its profile and the viewers' paths against
theirs.

### Endpoints

`--publisher-region`, `--viewer-region` (repeatable), and `--region` each
accept one of:

- a region slug from the table below;
- `default`: the `--url` / `LIVEKIT_URL` endpoint unchanged, which LiveKit routes to
  the cluster nearest the client;
- a full `wss://` URL, for anything not in the table.

| slug (aliases) | location | static IPs |
|---|---|---|
| `us` | US | yes |
| `eu` | France, Germany | yes |
| `canada` (`ca`) | Canada | yes |
| `india` | India | yes |
| `japan` | Japan | yes |
| `uk` | London | no |
| `aus` (`australia`) | Sydney | no |
| `saudiarabia` (`sa`) | Jeddah | no |
| `uae` (`dubai`) | Dubai | no |
| `israel` (`il`) | Jerusalem | no |
| `singapore` | Singapore | no |
| `brazil` | São Paulo | no |
| `mexico` | Querétaro | no |
| `southafrica` (`africa`) | Johannesburg | no |
| `zurich` (`switzerland`) | Zurich | no |

The first five map to `<project>.<slug>.rtc.livekit.cloud`. LiveKit has no `.rtc` name for
most of the others, so those slugs are pinned to one zone host from the
[firewall list](https://docs.livekit.io/deploy/admin/firewall/) (for example,
`uae` is `<project>.odubai1a.production.livekit.cloud`). Each run checks the region the
server reports, and a mismatch fails the run. Unknown slugs are rejected: LiveKit's DNS
resolves any unknown name to the nearest cluster, so a typo would otherwise measure the
wrong region. Region groups such as `me` (Saudi Arabia + UAE) are not slugs; pick one
region.

### Stages

| stage | from → to | includes |
|---|---|---|
| capture → encoder | capture → `EncoderUpload` | queueing before encode |
| encode | `EncoderUpload` → `EncoderOutput` | encoder |
| encoder → packetize | `EncoderOutput` → `WebrtcPacketize` | handoff to RTP |
| network | `WebrtcPacketize` → `WebrtcReceive` | pacer, network, SFU, NACK/RTX, reassembly |
| receive transform | `WebrtcReceive` → `DecoderUpload` | metadata strip (≈0) |
| jitter buffer + decode | `DecoderUpload` → `DecoderOutput` | libwebrtc frame buffer / playout delay, decode |
| decoded → app | `DecoderOutput` → `FrameSink` | delivery to the application |
| **TOTAL** | capture → `FrameSink` | everything except rendering/display |
| render | `FrameSink` → `GpuComplete` | `--display` only: drawing the frame in the window |
| **TOTAL capture → rendered** | capture → `GpuComplete` | `--display` only: until the frame is drawn, ready for the next screen refresh |

Percentiles are computed separately for each stage, so they don't add up to the total.
`dropped` is the percentage of published frames in the measurement window that
were never delivered as decoded frames.

### Options

- **Video:**
  - `--source pattern|camera` and `--camera-index N`. Cameras work natively only.
  - `--pattern moving|bars`.
  - `--codec vp8|vp9|av1|h264|h265`.
  - `--encoder`: `software` (the default; works everywhere, including Docker),
    `videotoolbox` on macOS, or `nvenc`/`vaapi` on Linux with the hardware.
  - `--width/--height/--fps/--max-bitrate`, and `--burn-timestamp` to draw the
    timestamp into the video.
- **Playout:**
  - `--min-playout-delay 0 --max-playout-delay 10` is the default. The room is
    recreated with these settings. `--max-playout-delay server` keeps the
    room's existing configuration.
  - `--low-latency` forces zero playout delay on the viewer.
- **Display:** `--display` opens the publisher preview and the viewer window,
  each with live stats above the video (press `I` in a window to hide them). Everything is
  still recorded, and the summary adds render time. Native only. The windows open
  side by side; with `--burn-timestamp`, a screenshot of both shows how far the
  viewer is behind (accurate to about one frame; average several screenshots).
  On a Linux box with a dummy display and remote desktop (seen with NVIDIA, X11 and
  RustDesk), the viewer can stop receiving frames when both windows are open. The
  viewer then exits 5 s after the run ends and the iteration is reported as failed.
  Headless runs are unaffected.
- **Network:**
  - `--firewall-profile` (Docker with `--cap-add NET_ADMIN`; default `open`)
    restricts the container's egress. Every profile allows TCP/443 to the signaling
    hosts' addresses, resolved once at startup and logged per endpoint (LiveKit's
    firewall rule for signaling is by hostname). The `static-*` profiles also allow
    LiveKit Cloud's static CIDRs (`cloud-static-ips.txt`): TCP/443 for TURN/TLS, plus
    the profile's ICE/TURN ports. `outbound-turn-udp` allows only UDP 3478 to any
    address, plus signaling.
    Every profile also allows DNS (port 53) to the nameservers in the container's
    `/etc/resolv.conf`.
  - Static IPs matter only for direct ICE and TURN/UDP, which go to the media server
    itself. LiveKit publishes them for `us`, `eu`, `canada`, `india` and `japan`. In
    other regions those paths are blocked under `static-*`, and media relays over
    TURN/TLS on TCP/443 through `<project>.turn.livekit.cloud`. That host resolves to
    the client's nearest cluster, so this works when that cluster is in a static
    region.
  - `--relay-only` requires TURN relay candidates.
  - `--data-probe` also measures data-channel round trips.

Run `lk-latency <mode> --help` for everything else. Internal roles are also
available: `lk-latency publisher --help` and `lk-latency subscriber --help`.

### Accuracy notes

- All processes in a measurement must share a clock: one host, or one Docker VM.
  Synchronizing clocks across machines isn't handled yet.
- Docker Desktop on macOS adds some UDP overhead through its network layer, so
  absolute numbers are slightly higher than native. Comparisons between regions
  remain valid.
- A low max playout delay (e.g. 10 ms) can make viewers drop 10–30% of frames;
  check `dropped` and `fps`.

## Build

```sh
scripts/build-linux.sh amd64            # or arm64; uses the Dockerfile `binary` stage
docker build --target image -t lk-latency .   # image from dist/linux-<arch>/
scripts/build-macos.sh                  # on a Mac (brew install pkg-config jpeg-turbo)
scripts/package.sh v0.1.0               # tarballs + SHA256SUMS from dist/
```

CI: `.github/workflows/release.yml` (GitHub) and `.gitlab-ci.yml` (GitLab) call the
same scripts. Pushing a `v*` tag publishes release tarballs and versioned image tags.
