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
