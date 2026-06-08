# blitzer

`blitzer` is a Rust terminal app (TUI) for high-throughput downloads using parallel HTTP range requests.

## Features

- Parallel chunk downloads (`Range` requests)
- Automatic connection planning with worker pooling and segmented load balancing
- Speculative overlap-verified parallel downloads for no-range servers, with safe single-stream fallback
- Resume support via manifest-verified part files
- Per-chunk retry with exponential backoff
- Full-screen interactive TUI (no startup flags required)
- Live in-app progress with throughput and mode details

## Build

```bash
cargo build --release
```

## Usage

```bash
./target/release/blitzer
```

You will get an interactive form for:

- URL
- Output path (optional)
- Connections (`auto` or `1..=64`)
- Retries (`<=20`)
- Timeout seconds (`5..=300`)
- Resume toggle

For automated downloads:

```bash
./target/release/blitzer "https://example.com/file.iso" --output ./downloads/file.iso --connections auto
```

CLI options: `--connections auto|N`, `--retries N`, `--timeout SECONDS`, `--no-resume`, `--no-range-strategy single|overlap`, `--no-range-workers N`, and `--overlap-bytes N`.

No-range servers default to the speculative overlap strategy:

```bash
./target/release/blitzer "https://example.com/file" --no-range-strategy overlap --no-range-workers 4 --overlap-bytes 65536
```

Use `--no-range-strategy single` to force the conservative single-stream fallback.

## TUI controls

- `Tab` / `Shift+Tab`: Move between fields
- `Left click`: Focus a field, place cursor in text inputs, toggle resume checkbox
- `Left` / `Right` / `Home` / `End`: Move cursor within current input field
- `Backspace` / `Delete`: Remove text around cursor in current input field
- `Ctrl+U`: Clear the focused input field
- `Enter`: Start download (or return from done/error screen)
- `Space`: Toggle resume mode when resume field is focused
- `c` / `Esc`: Cancel an active download and keep the result on screen
- `q` / `Ctrl+C`: Cancel an active download, save verified resume data, then quit
- `q` / `Esc`: Quit from result screens

## Notes

- Best performance still comes from servers that support byte-range requests.
- When a server does not support `Range` requests, Blitzer can start ordinary streams, skip forward inside each stream, and verify overlapping bytes before merging. For unknown-size responses it first checks whether the body fits in the initial payload window before starting extra streams; if the overlap proof fails because the source is dynamic or unstable, Blitzer falls back to a single stream.
- If a server rejects high range concurrency, Blitzer preserves verified parts and retries with four workers, then one worker if needed, instead of switching to multi-stream no-range mode.
- Some download pages gate the real file behind browser-style navigation. When a redirected HTML page exposes a referer-gated attachment, Blitzer reprobes the original URL with the discovered referer and downloads the actual file.
- Resume data is tied to the URL, content validators, total size, and chunk layout. Stale or legacy part files are discarded instead of being merged into a corrupted output.
- `auto` selects a worker count from file size and available CPU, then splits the file into more segments than workers so fast lanes keep pulling work while slower ranges do not hold the whole download hostage.
- For very old or low-resource systems, use a fixed low connection count such as `4` or `8` and tune up/down based on CPU usage and network behavior.
