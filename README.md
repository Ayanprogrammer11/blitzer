# blitzer

`blitzer` is a Rust terminal app (TUI) for high-throughput downloads using parallel HTTP range requests.

## Features

- Parallel chunk downloads (`Range` requests)
- Automatic fallback to single-stream mode when range requests are unsupported
- Resume support via part files
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
- Connections (`1..=64`)
- Retries (`<=20`)
- Timeout seconds (`5..=300`)
- Resume toggle

## TUI controls

- `Tab` / `Shift+Tab`: Move between fields
- `Left click`: Focus a field, place cursor in text inputs, toggle resume checkbox
- `Left` / `Right` / `Home` / `End`: Move cursor within current input field
- `Backspace` / `Delete`: Remove text around cursor in current input field
- `Enter`: Start download (or return from done/error screen)
- `Space`: Toggle resume mode when resume field is focused
- `c`: Cancel an active download
- `q`: Quit from result/download screens
- `Esc`: Quit from form/result screens
- `Ctrl+C`: Quit immediately

## Notes

- Best performance requires servers that support byte-range requests.
- For very old or low-resource systems, start with `-n 8` and tune up/down based on CPU usage and network behavior.
