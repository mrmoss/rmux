# rmux (Rust)

Rust port of the supplied Python `mmux` terminal multiplexer.

The port keeps the same core design: each pane owns a PTY, raw PTY output is retained as the source of truth, and the virtual terminal is rebuilt from that history when a pane changes dimensions or is swapped. The Python project documents the same architecture and keybindings.

## Build

Requires a Rust toolchain with Cargo.

```bash
cargo build --release
```

The binary is:

```text
target/release/rmux
```

Or:
```bash
cargo build --release --target x86_64-unknown-linux-musl
```

The binary is:

```text
target/x86_64-unknown-linux-musl/release/rmux
```

## Usage

```bash
./target/release/rmux [options]
```

Options:

- `-s, --shell <SHELL>` — shell command; defaults to `$SHELL` or `/bin/bash`
- `-n, --no-instructions` — hide the footer
- `-d, --debug` — show the last captured key event
- `--min-width <N>` — minimum pane width, default `19`
- `--min-height <N>` — minimum pane height, default `10`
- `-V, --version` — show the version

Keybindings:

- `Ctrl+V` — split vertical
- `Ctrl+H` — split horizontal
- `Ctrl+Space` — toggle zoom
- `Ctrl+B` - toggle broadcast mode
- `Ctrl+Arrow` — navigate
- `Ctrl+Shift+Arrow` — resize
- `Alt+Shift+Arrow` — swap pane terminal state
