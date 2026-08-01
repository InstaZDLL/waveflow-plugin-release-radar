# WaveFlow — Release Radar plugin

A [WaveFlow](https://github.com/InstaZDLL/WaveFlow) plugin that surfaces **recent releases from the artists in your library**, sourced from [MusicBrainz](https://musicbrainz.org) with covers from the [Cover Art Archive](https://coverartarchive.org).

It's a sandboxed WebAssembly component implementing the `waveflow:ui/v1` world — one portable `plugin.wasm`. It renders a custom sidebar view **without shipping any React**: the plugin returns a declarative JSON *view descriptor* (sections / cards / buttons) that WaveFlow draws with its own native components. That's the security boundary — a plugin can never inject HTML, CSS, or JS into the app.

## What it does

1. Reads the **redacted artist list** the host exposes (`library.list-artists` — artist names + aggregate track counts + opaque ids only; **no file paths, no per-track rows**), following your top artists by track count. This read is local-only, so it works offline.
2. For each artist, queries the **MusicBrainz** release-group search for releases in the last ~6 months.
3. Renders the matches as cards: cover (Cover Art Archive), title, artist, date, and type — each linking out to its MusicBrainz page.

There is **no playback** and **no YouTube** — Release Radar is a pure discovery view. It never downloads or streams audio.

## How the scan works

MusicBrainz asks clients to average **≤ 1 request/second**, and the host caps a single plugin call at ~30 s. So the scan is **incremental**:

- A "scan" processes a small batch of artists, spacing requests ~1.1 s apart.
- On a `503`/`429` (rate-limited) it **stops cleanly** and leaves a cursor — it never hammers through a rate-limit response.
- Results + the cursor are cached in the plugin's private scratch store, so re-opening the view is instant and **Continue** resumes where the last batch stopped.

The first full sweep of a large library therefore takes a few clicks; after that it's cached.

## Build

Needs [`cargo-component`](https://github.com/bytecodealliance/cargo-component) + the `wasm32-wasip1` target:

```bash
cargo component build --release
# -> target/wasm32-wasip1/release/waveflow_plugin_release_radar.wasm
```

`cargo component` wraps the module into a true wasip2 Component.

## Install

Users don't build it — they install it from WaveFlow's in-app plugin store (Settings → Plugins → Store) once it's listed in [InstaZDLL/waveflow-plugins](https://github.com/InstaZDLL/waveflow-plugins). Requires a WaveFlow build that ships the `waveflow:ui/v1` world.

## Permissions

Declared in [`manifest.toml`](manifest.toml) and enforced by the host sandbox:

- **HTTP** — `https://musicbrainz.org/**` only. Cover Art Archive images and the "View on MusicBrainz" links are loaded/opened by the app itself (the webview / your OS browser), not through the plugin, so they aren't on the allowlist.
- **Scratch store** — its own 10 MB private key/value store (the release cache + scan cursor). No filesystem access.
- **Library (redacted artist read)** — artist names + counts + opaque ids only. No file paths, no track listings.

## Caveats

- **Coverage depends on MusicBrainz.** An artist mistagged in your library (or a homonym) may match poorly; releases MusicBrainz hasn't indexed yet won't appear.
- **Covers are best-effort.** A release-group with no Cover Art Archive entry shows a placeholder glyph.

## License

[MIT](LICENSE).
