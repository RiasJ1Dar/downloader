[Українська](README.md) · **English**

# Downloader

A download manager for Windows: segmented downloading with resume, HLS/DASH,
YouTube. In the vein of IDM and Ant Download Manager.

![Architecture](docs/architecture.en.png)

Interactive diagram: [open in a browser](https://riasj1dar.github.io/downloader/architecture.en.html)
(GitHub shows the file as code, not as a page). Source — `docs/architecture.en.json`.

## What it does

- **Segmented downloading** with dynamic splitting: a free worker takes
  **half of what is left** in the slowest segment, so one slow tail no longer
  holds up the whole task.
- **Resume after a broken connection** — and after the process is killed.
  The state file lives next to the data, and the write order is
  data → `sync` → state. The state therefore lags behind the data and never
  runs ahead of it.
- **HLS and DASH**: quality picked from the manifest, segments merged,
  separate audio tracks muxed into one file.
- **YouTube** through yt-dlp, quality selection included.
- **Browser extension**: intercepts downloads, shows a button on `<video>`,
  picks up `.m3u8` and `.mpd` from the page.
- **A queue** with speed limits, scheduling and a post-action (sleep,
  shutdown).
- **Interfaces**: an Avalonia window with a segment bar and a speed graph, a
  command line, Ukrainian and English.

## Installation

Two packages, differing only in ffmpeg:

| | size | when to take it |
|---|---|---|
| `Downloader-X.Y.Z-web.msi` | 43 MB | the usual case: `dl ffmpeg-install` fetches ffmpeg later |
| `Downloader-X.Y.Z-full.msi` | 95 MB | a machine with no network, or offline distribution |

Installs **per user**, into `%LOCALAPPDATA%\Programs\Downloader`, with no UAC
prompt and no service. The window ships .NET inside — no runtime to deliver.

⚠️ The packages are **not signed**: there is no code-signing certificate, so
SmartScreen will warn on first run. Instead, every release ships a
`SHA256SUMS.txt`, and the packages themselves are built in public CI from this
same source — so anyone can rebuild them and check the sums:

```
sha256sum -c SHA256SUMS.txt
```

### Why ffmpeg is needed

Without it YouTube does not download at all. Video and audio are served as
**separate tracks** — not even 144p exists as a single file — and nothing else
here can mux them. DASH likewise serves audio as its own stream.

The build used is the **LGPL** one, without `enable-gpl`. That is not a
detail: the GPL variant would oblige us to hand the source of the whole
program to everyone who receives it. LGPL carries no such requirement, and
ffmpeg here is not even linked into the code — it runs as a separate process.
Its licence ships alongside, in `LICENSE-ffmpeg.txt`.

## Command line

The console client is `dl`. It can do everything the window can. Quick start:

```
dl get https://example.com/file.zip -o file.zip
```

`dl get` downloads by itself, in this console. The other commands drive the **core**
(`downloader-core`, lives in the tray): a task handed over with `dl add` outlives the console.

```
downloader-core
dl add https://example.com/file.zip
dl variants https://cdn.example/master.m3u8
dl get https://cdn.example/master.m3u8 -o video.mp4 --variant 720p.ts
dl list
dl watch
```

### Commands

| Command | What it does |
|---|---|
| `dl get <URL>` | download a file right in this process |
| `dl probe <URL>` | show what is known about a link without downloading |
| `dl variants <URL>` | list quality variants (HLS, DASH, YouTube); no core needed |
| `dl add [URL]` | hand a download over to the core |
| `dl list` | list the core's tasks |
| `dl parts <ID>` | show how a task is split into parts (what the segment bar shows) |
| `dl pause <ID>` | pause a task |
| `dl resume <ID>` | resume a paused task |
| `dl rm <ID> [--with-file]` | remove a task from the list; `--with-file` also deletes the downloaded bytes |
| `dl move <ID> -q <QUEUE>` | move a task to another queue |
| `dl watch` | follow progress until Ctrl+C |
| `dl settings` | show the core's rules: queue, limit, schedule, after-action |
| `dl configure …` | change the core's rules |
| `dl queue …` | manage named queues |
| `dl ffmpeg-install` | install ffmpeg next to the program |
| `dl ytdlp-update` | update yt-dlp (`yt-dlp -U`) |
| `dl update` | check for a newer version (check only, installs nothing) |
| `dl completions <SHELL>` | generate shell completion: `pwsh`, `bash`, `zsh`, `fish`, `elvish` |

`<ID>` is an id from `dl list`. `dl --version` prints the version, `dl <command> --help` the help.

### Options

Common to every command: `--lang uk|en`.

**`dl get <URL>`**

| Option | Default | Meaning |
|---|---|---|
| `-o, --out <FILE>` | — | where to save; without it the name comes from `Content-Disposition` or the URL |
| `-n, --parts <N>` | `8` | how many connections to open |
| `--min-chunk <BYTES>` | `1048576` | smallest chunk per connection |
| `--limit-kb <KB/s>` | `0` | speed cap; `0` means unlimited |
| `--checkpoint-ms <MS>` | `2000` | how often the state is flushed to disk |
| `--cookie <STRING>` | — | Cookie header: `n=v; n2=v2` |
| `--referer <URL>` | — | Referer header |
| `--variant <ID>` | — | quality variant: a value from the first column of `dl variants` |
| `--duration <TIME>` | — | duration cap for live streams: `30s`, `10m`, `1h` |
| `--rewind` | — | start a live stream from the beginning of its buffer (DVR) |

**`dl add [URL]`** — the URL may be a pattern `file[001-010].zip`, which expands into a list.

| Option | Meaning |
|---|---|
| `--list <FILE>` | file with URLs (one per line, `#` starts a comment) |
| `--clipboard` | take URLs from the clipboard |
| `-o, --out <FILE>` | where to save; ignored for several URLs |
| `-n, --parts <N>` | how many connections |
| `-q, --queue <QUEUE>` | queue, `default` by default |
| `--cookie`, `--referer`, `--duration`, `--rewind` | as in `dl get` |

**`dl variants <URL>`** accepts `--cookie` and `--referer`.

**`dl configure`** — an omitted option leaves that field unchanged.

| Option | Meaning |
|---|---|
| `--max <N>` | how many tasks run at once |
| `--rate-kb <KB/s>` | speed cap; `0` means unlimited |
| `--after <ACTION>` | once the queue is empty: `none`, `sleep` or `shutdown` |
| `--from <HH:MM>`, `--to <HH:MM>` | window in which tasks may start |
| `--clear-schedule` | remove the schedule (download any time) |
| `--quiet-from <HH:MM>`, `--quiet-to <HH:MM>` | night speed profile |
| `--quiet-kb <KB/s>` | night limit; `0` disables the night profile |
| `--clear-quiet` | remove the night profile |

**`dl queue`**

| Command | Meaning |
|---|---|
| `dl queue list` | list queues |
| `dl queue add <NAME> [options]` | create a queue |
| `dl queue set <NAME> [options] [--clear-schedule]` | change a queue's settings |
| `dl queue pause <NAME>` / `resume <NAME>` | pause / resume every task in the queue |
| `dl queue rename <OLD> <NEW>` | rename |
| `dl queue rm <NAME>` | delete; its tasks move to `default` |

Queue options: `--max <N>`, `--rate-kb <KB/s>`, `--from <HH:MM>`, `--to <HH:MM>`,
`--after none|sleep|shutdown`. The `default` queue cannot be deleted or renamed.

**`dl update`**

| Option | Meaning |
|---|---|
| `--check [true\|false]` | only check and verify hashes (`true` by default) |
| `--manifest <PATH\|URL>` | local file or URL of the update manifest |
| `--verify-file <FILE>` | verify a downloaded file's SHA-256 against the manifest |

**Completion** for PowerShell:

```
dl completions pwsh | Out-String | Invoke-Expression
```

### Environment variables

| Variable | Meaning |
|---|---|
| `DOWNLOADER_LANG` | `uk` or `en`; same as `--lang` |
| `DOWNLOADER_PROXY` | HTTP proxy (`http://…` or `socks5://…`); `none` or empty means no proxy. Without it `HTTPS_PROXY` / `HTTP_PROXY` are used |
| `DOWNLOADER_MAX_CONCURRENT` | core: tasks at once, overrides the saved setting |
| `DOWNLOADER_RATE_LIMIT` | core: speed cap in **bytes** per second, overrides the saved setting |
| `DOWNLOADER_WATCH_CLIPBOARD` | `0` stops the core from watching the clipboard |
| `DOWNLOADER_POST_DELAY_MS` | delay before the after-action (sleep / shutdown), 60000 by default |
| `DOWNLOADER_POST_ACTION_DRY` | `1` only simulates the after-action (for testing) |

Without the option or variable the language follows the OS; a Russian or any other OS gets Ukrainian.

### Where data lives

| What | Normal mode | Portable mode |
|---|---|---|
| Program | `%LOCALAPPDATA%\Programs\Downloader` | any folder containing `portable.txt` |
| Tasks and settings | `%LOCALAPPDATA%\Downloader\tasks.db` | `<program folder>\tasks.db` |
| Window theme | `%LOCALAPPDATA%\Downloader\ui.json` | `<program folder>\ui.json` |
| Downloads | `%USERPROFILE%\Downloads` | `<program folder>\Downloads` |

Portable mode is switched on by an empty `portable.txt` next to `dl.exe`, `downloader-core.exe` and
`Downloader.Ui.exe`. More in [INSTALL.txt](INSTALL.txt) (Ukrainian).

### Core and native host

```
downloader-core [--db <FILE>] [--downloads <DIR>] [--pipe <NAME>] [--lang uk|en]
```

| Option | Meaning |
|---|---|
| `--db <FILE>` | another task database file |
| `--downloads <DIR>` | where files without an explicit path go |
| `--pipe <NAME>` | another channel name — for tests and several independent instances |
| `--lang uk\|en` | language of the tray and messages |

The core's log level is set with `RUST_LOG` (`info` by default).

The browser extension talks to the core through `downloader-nmhost`. The core registers it on every
start; by hand: `downloader-nmhost --install [--allow-extension <ID>]`, where `--allow-extension`
allows one more extension ID (repeatable). Instructions: [ext/INSTALL.txt](ext/INSTALL.txt).

## Building

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

The window (.NET 10 SDK required):

```
dotnet build -c Release apps/ui
```

Installers (WiX 6 required — `dotnet tool install --global wix`):

```
pwsh -File packaging/zibraty.ps1 -Version 0.1.0
```

For the full package, put ffmpeg into `tools/ffmpeg-dist`.

## Layout

```
crates/core         engine, scheduler, protocol registry; knows nothing of the UI
crates/proto-http   segments, If-Range, retries
crates/proto-hls    playlists, tracks, decryption
crates/proto-dash   MPD: SegmentList and SegmentTemplate
crates/proto-ytdlp  YouTube through external yt-dlp
crates/ffmpeg       locating ffmpeg, remuxing, merging tracks
crates/ipc          local channel: named pipe / unix socket
crates/i18n         translation catalogues shared by every shell
apps/core-service   the core service: queue, events, tray
apps/cli            command line
apps/ui             Avalonia window
apps/nmhost         native messaging host for the extension
ext/chrome          browser extension
```

## The rules that hold the architecture together

1. **The core never names a protocol.** HTTP, HLS, DASH, torrent — all of it
   goes through the `Protocol` contract. A test with a dummy protocol proves
   it.
2. **Swallowing an error is forbidden.** `unwrap`, `expect`, `panic` and
   `let _ =` in the core are rejected by the build (`[lints.clippy]`).
3. **Logic does not creep into the UI.** Everything the window can do, the
   command line can do first.
4. **An error names the symptom.** Not «invalid state», but «the resource
   changed, resuming is impossible».
5. **The channel is local only.** A named pipe or a unix socket, never a TCP
   port — not even on `127.0.0.1`.

Comments, error messages and test names are written in Ukrainian; type,
function and field names in English, as is customary in Rust.

## Contributing

Open an issue first, then write code: a large change that does not fit the
project's intent will be turned down regardless of code quality.

Contributions are accepted with the
[Contributor Agreement](https://github.com/RiasJ1Dar/.github/blob/main/CLA.md)
accepted: you keep the copyright to your work, and the project gains the right
to release it under different terms. Without that, relicensing would be
impossible without permission from everyone who ever sent a patch. The rest of
the rules are in
[CONTRIBUTING](https://github.com/RiasJ1Dar/.github/blob/main/.github/CONTRIBUTING.md).

## Licence

[GNU General Public License v3.0 or later](LICENSE).

You are free to use the program, study it and share it. If you distribute a
modified version, the source of your changes must be open under the same
licence. This code cannot be turned into a closed product.

Third-party components — Rust and .NET libraries, ffmpeg, yt-dlp — belong to
their authors and come under their own licences: see
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). None of them conflicts
with the GPL: they are all either permissive or MPL-2.0.
