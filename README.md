# blitzer

`blitzer` is a Rust CLI downloader that uses multiple HTTP range requests in parallel to maximize throughput.

## Features

- Parallel chunk downloads (`Range` requests)
- Automatic fallback to single-stream mode when range requests are unsupported
- Resume support via part files
- Per-chunk retry with exponential backoff
- Progress bar and transfer speed output

## Build

```bash
cargo build --release
```

## Usage

```bash
# Basic usage (auto output name)
./target/release/blitzer "https://example.com/file.iso"

# Save to specific output path
./target/release/blitzer "https://example.com/file.iso" -o ./downloads/file.iso

# Increase parallelism
./target/release/blitzer "https://example.com/file.iso" -n 16

# Disable resume behavior
./target/release/blitzer "https://example.com/file.iso" --no-resume
```

## CLI options

- `-o, --output <PATH>`: Output file path
- `-n, --connections <N>`: Number of parallel connections (`1..=64`)
- `--retries <N>`: Retry attempts per chunk (`<=20`)
- `--timeout-secs <N>`: Request timeout in seconds (`5..=300`)
- `--no-resume`: Ignore existing part files and restart from scratch

## Notes

- Best performance requires servers that support byte-range requests.
- For very old or low-resource systems, start with `-n 8` and tune up/down based on CPU usage and network behavior.
