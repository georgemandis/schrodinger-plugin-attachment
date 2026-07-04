# schrodinger-plugin-attachment

A [Schrödinger](https://clipboard.cat) derivation plugin that turns the current
clipboard entry into a **file attachment**. When this derivation is active,
pasting into an app that accepts files (Mail, Slack, Finder/Explorer, chat apps)
drops the clip in as a file instead of inline text or an image.

## What it does

`apply()` detects the clip's content, writes it to a temp file, and puts the
platform's **file-reference** clipboard format on the clipboard pointing at that
file. Auto-detected kinds (in priority order):

| Clip content        | File produced |
|---------------------|---------------|
| PNG / JPEG / TIFF   | `.png` / `.jpg` / `.tiff` |
| HTML                | `.html` |
| Plain text          | `.txt` |

The file name is derived from the clip (a short slug of text content, plus the
change-count to keep successive files distinct), e.g. `meeting-notes-1234.txt`.

## Why it's a native plugin

Producing a file attachment requires a real file on disk — the receiving app
opens the referenced path. Scripted (JS) plugins run in a sandbox with no
filesystem access, so this has to be a native plugin (it runs in-process with
full `std` access, like the OCR and Faces plugins).

## Platform file-reference formats

The plugin emits the same formats Schrödinger/copycat read back:

- **macOS** — `public.file-url` (a single `file://` URL, percent-encoded)
- **Linux** — `text/uri-list` (RFC 2483, CRLF-terminated `file://` lines)
- **Windows** — `CF_HDROP` (a `DROPFILES` blob, UTF-16LE, double-NUL terminated)

## Temp file lifetime

Attachment files are written to `<system-temp>/schrodinger-attachments/` and are
**not** auto-deleted on `apply()` return — they must survive until you paste.
Each run sweeps files in that directory older than ~1 hour, so it doesn't grow
without bound. If you need a file to persist longer than an hour, paste it before
then.

## Building

```sh
cargo build --release          # produces the cdylib for your platform
cargo test                     # unit tests for detection + encoding
```

Releases are built for macOS (arm64), Linux (x86_64), and Windows (x86_64) by
the tag-triggered GitHub Actions workflow, matching the registry's expected
`{id}-{version}-{platform}.tar.gz` layout.

## Known limitations / follow-ups

- Only the first/best format is attached (no multi-file drops).
- Windows and Linux paths are runtime-untested from the authoring machine
  (macOS); the encodings match copycat's decoders and are covered by unit tests,
  but verify in a real session before relying on them.
