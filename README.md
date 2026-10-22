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

Never put credentials in an image or a repo. Keep them in a private file:

```sh
mkdir -p ~/.config/lk-latency && cat > ~/.config/lk-latency/cloud.env <<'EOF'
LIVEKIT_URL=wss://<project>.livekit.cloud
LIVEKIT_API_KEY=...
LIVEKIT_API_SECRET=...
EOF
chmod 600 ~/.config/lk-latency/cloud.env
```

## Install

**Docker** (macOS or Linux):

```sh
IMG=ghcr.io/<owner>/lk-latency:latest
docker run --rm --env-file ~/.config/lk-latency/cloud.env -v "$PWD/results:/results" $IMG run --viewer-region eu
```

**Native** binary from the releases page:

```sh
curl -fsSL -o lk.tgz https://github.com/<owner>/lk-latency/releases/download/<version>/lk-latency-<version>-macos-arm64.tar.gz
tar -xzf lk.tgz && ./lk-latency version
set -a; . ~/.config/lk-latency/cloud.env; set +a
```

The macOS binary is unsigned. Downloading it with `curl` avoids Gatekeeper's
quarantine. If you downloaded it in a browser, clear the flag with
`xattr -d com.apple.quarantine lk-latency`. The Linux binary needs a glibc 2.35+
distro (Ubuntu 22.04+, Debian 12+).

## Measure

```sh
# Publisher in EU, simultaneous viewers in EU and US, 5 fresh-room iterations
lk-latency run --publisher-region eu --viewer-region eu --viewer-region us --duration 30 --iterations 5

# Native only: real camera, hardware encoder, and windows with timestamp overlays
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

### Endpoints

`--publisher-region`, `--viewer-region` (repeatable), and `--region` each
accept one of:

- a region slug: `eu`, `us`, `india`, `japan`, `canada`, `uae`, …, which maps to
  `<project>.<slug>.rtc.livekit.cloud` (`uae` maps to the Dubai cluster);
- `default`: `LIVEKIT_URL` unchanged;
- a full `wss://` URL.

For known slugs the server-reported region is verified, and a mismatch fails the run.

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
- **Display:** `--display` opens the publisher preview and the viewer window
  with timestamp and latency overlays, and still records everything. Native
  only.
- **Network:**
  - `--firewall-profile` (Docker with `--cap-add NET_ADMIN`; default `open`)
    restricts the container's egress. The `static-*` profiles allow only LiveKit
    Cloud's static CIDRs; `outbound-turn-udp` allows only UDP 3478 plus
    signaling.
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
