# Benchmarks

Measures Portal's video and control latency, and the same scenarios on the
plain LiveKit Python SDK for comparison. Results are in
[docs/reference/benchmarks.md](../../docs/reference/benchmarks.md).

- `portal_bench.py` runs the scenarios through Portal's Python API.
- `sdk_bench.py` runs them through `livekit.rtc`.
- `charts.py` turns both into the SVG charts and tables in the docs.

Each scenario runs the robot and the operator as separate processes in a fresh
room, starting together. A run warms up for 8 s and measures for 10 s, at 30 fps.
Latency is the receiver's wall clock minus the timestamp the sender put on the
frame or packet, so both processes need the same clock: run them on one host.

## Run

Start a local server, build the release FFI, and install the SDK next to Portal:

```bash
livekit-server --dev
bash scripts/build_ffi_python.sh release
cd python && uv sync --group dev
```

Then, from `scripts/bench`, run each side three times and render:

```bash
for i in 1 2 3; do
  uv run --project ../../python python portal_bench.py suite --label portal-r$i \
    --size-hint --codecs H264,VP8,VP9,AV1 --out results/portal-r$i.json
  uv run --project ../../python python sdk_bench.py suite --label sdk-r$i \
    --out results/sdk-r$i.json
done
uv run --project ../../python python charts.py \
  --portal results/portal-r*.json --sdk results/sdk-r*.json \
  --out ../../docs/reference/benchmarks
```

`--only video|control|mixed`, `--codecs` and `--res` narrow a run. `--codecs
MJPEG,PNG,RAW` covers Portal's byte-stream codecs, which the SDK has no
equivalent for.
