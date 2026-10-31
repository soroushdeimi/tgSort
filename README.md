# tgsort

A small macOS tool that watches Telegram Desktop's download folder
(`~/Downloads/Telegram Desktop`) and moves finished downloads into organized
folders according to rules you define. A file is only touched after its size and
modification time have been stable for `settle_secs`, so in-progress downloads
are left alone.

With the example config, everything from Telegram ends up in one place:

```
~/Telegram/
├── Movies/2026-10/     mp4, mkv, mov, …
├── Series/<name>/      Show.S01E02.mkv
├── Music/              mp3, flac, ogg (voice messages), …
├── Docs/pdf/ docx/ …
├── Code/txt/ html/ md/ …
├── Archives/zip/ rar/ …
├── Pictures/2026-10/
└── Other/<ext>/        anything no rule matches (no-ext/ for files without one)
```

## Install

Download `tgsort-<tag>-macos-universal.tar.gz` (and its `.sha256`) from the
[latest release](https://github.com/soroushdeimi/tgSort/releases), then:

```sh
shasum -a 256 -c tgsort-*-macos-universal.tar.gz.sha256
tar -xzf tgsort-*-macos-universal.tar.gz
# the binary isn't notarized, so Gatekeeper blocks it unless you clear the quarantine flag
xattr -d com.apple.quarantine tgsort 2>/dev/null || true
mkdir -p ~/.local/bin && rm -f ~/.local/bin/tgsort && cp tgsort ~/.local/bin/
~/.local/bin/tgsort init
~/.local/bin/tgsort check
~/.local/bin/tgsort install
```

Or build from source (see below).

## Build

```sh
cargo build --release
# binary: target/release/tgsort
mkdir -p ~/.local/bin
rm -f ~/.local/bin/tgsort && cp target/release/tgsort ~/.local/bin/
```

Always `rm` before copying an updated binary: overwriting a running, signed
binary in place makes macOS kill it (`OS_REASON_CODESIGNING`). After updating,
restart the agent with `launchctl kickstart -k gui/$(id -u)/com.tgsort.agent`.

## Use

> **Warning:** the first `run` (or `install`) organizes every file that is
> *already* in the watch folder, not just new downloads. Before starting it, run
> `tgsort check` and `tgsort once --dry-run` and review what would be moved.

```sh
tgsort init        # writes ~/.config/tgsort/config.toml (use --force to overwrite)
$EDITOR ~/.config/tgsort/config.toml
tgsort check       # validate config and preview file -> rule -> destination; moves nothing
tgsort once --dry-run   # log what a single pass would do
tgsort once        # process existing settled files once
tgsort run         # run the watcher in the foreground
tgsort install     # install and load a LaunchAgent (starts at login, restarts on crash)
```

Global flags: `--config <PATH>` to use another config file, `-v` for debug logs.
`run` and `once` accept `--dry-run`.

## Logs

The LaunchAgent writes stdout/stderr to `~/Library/Logs/tgsort.log`:

```sh
tail -f ~/Library/Logs/tgsort.log
```

## Uninstall

```sh
tgsort uninstall   # unloads and removes ~/Library/LaunchAgents/com.tgsort.agent.plist
```

Optionally delete `~/.config/tgsort/` and the log file as well.

## AI classification (optional)

`tgsort-ai` is a small helper that lets tgsort ask Apple's on-device language
model to pick the best folder for a file, using its name plus a short excerpt of
its content (the first two pages of a PDF, or the start of a text file).

- **Requirements:** Apple Silicon, macOS 26 or newer, Apple Intelligence turned
  on (System Settings > Apple Intelligence & Siri). Intel Macs are not supported.
- **Privacy:** everything runs on the device through Apple's Foundation Models
  framework. File names and contents are never sent anywhere.
- **Cost:** the helper is started only when needed and exits afterwards; nothing
  stays resident.

Install it next to tgsort (the release tarball contains it, or build it with
`cd helpers/tgsort-ai && swift build -c release --arch arm64`):

```sh
mkdir -p ~/.local/bin
rm -f ~/.local/bin/tgsort-ai && cp tgsort-ai ~/.local/bin/    # from the tarball
# from source: cp "$(swift build -c release --arch arm64 --show-bin-path)/tgsort-ai" ~/.local/bin/
xattr -d com.apple.quarantine ~/.local/bin/tgsort-ai 2>/dev/null || true
~/.local/bin/tgsort-ai --check      # {"available": true} when ready
```

Enable it in `~/.config/tgsort/config.toml` (see `config.example.toml` for the
current option names) and give each AI-sorted folder a short hint so the model
knows what belongs there. When the model is unsure or unavailable, files fall
back to your normal rules.
