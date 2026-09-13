# Browser terminal checkpoint scope

The raw PTY attach path uses an atomic output cursor and a checkpoint from
`taarof-app/src/pty_broker/screen.rs`. This is a bounded screen model, not a
complete VT implementation. Native VTE still consumes the original bytes.

The renderer corpus in `checkpoint_renderer_probe` compares original bytes with
checkpoint plus replay at every byte boundary against actual GTK/VTE. It checks
visible text, HTML attributes and a DSR cursor reply through a disposable PTY.
`taarof-web/scripts/checkpoint-xterm.mjs` runs the same bytes in browser xterm and compares
cells, widths, colors, bold/reverse attributes, cursor and active buffer. A
renderer difference fails the check; model self-comparison is not the oracle.
The corpus includes CSI cursor movement, the supported SGR attributes, alternate
screen entry/return, CJK width, combining text, OSC 2/7/8 with BEL/ST endings,
cancellation and repeated ESC, plus resize and forced replay-gap recovery.

OSC payloads are discarded with constant memory. Their title, working-directory
and hyperlink metadata are not reconstructed. Pending OSC/CSI/ESC framing and
incomplete UTF-8 are carried into checkpoint continuation so a replay suffix
cannot become printable control payload. Checkpoints restore the preceding
printed glyph when needed to allow a following combining mark to join it.

Supported width behavior uses GLib's non-ambiguous character width and one
continuation cell for a double-width character. The model's existing combining
mark ranges remain bounded; this is not a Unicode grapheme implementation.

Known limitations requiring a separate design decision or further corpus work:

- No claim for full Unicode grapheme clusters, ZWJ emoji, ambiguous-width policy,
  combining after arbitrary cursor repositioning, or renderer Unicode-version
  equivalence outside the corpus.
- Resize copies and clips cells. It does not implement primary-buffer reflow or
  recreate scrollback. The resize corpus uses short lines that do not reflow.
- SGR support is bold, reverse and the 16 ANSI foreground/background colors.
  Italic, underline, 256-color/true-color, styled erased blanks and many other
  attributes are not modeled completely.
- After `abc ESC[?1049h ALT ESC[?1049l U+0301 !`, VTE joins the accent
  to `c`, while xterm emits a separate combining cell and advances. Checkpoints
  before the combining mark (including its split UTF-8 prefix) preserve each
  renderer's behavior. Checkpoints after it has been modeled remain unsupported;
  the exact sequence is retained in `--unsupported` and fails the xterm check.
- SUB cancellation differs between the actual renderers: VTE displays a
  cancellation symbol and advances, while xterm ignores it. The explicit
  `checkpoint_renderer_probe --unsupported` corpus retains this as a failing
  differential check; both parser paths cancel OSC without exposing its payload.
- Scrolling regions, insert/delete character/line, tab-stop state, most ESC
  commands, DCS/APC/PM/SOS strings, eight-bit C1 controls and complete terminal
  mode state are not covered. OSC metadata is intentionally omitted.
- The projection hash covers the exposed screen projection, not all parser,
  pending UTF-8 or hidden-buffer state. It is not a complete terminal identity.

## Run the differential check

Build the example with the repository toolchain:

```sh
cargo build --manifest-path taarof-app/Cargo.toml --example checkpoint_renderer_probe
```

Run the built example in the Kasm image with an isolated X display and private
HOME/XDG paths; capture its stdout as `corpus.json`. It creates only disposable
GTK widgets and PTYs, never reads the installed app's runtime or terminal
history, and requires no HTTP token. Then, with Chromium and web dependencies
installed:

```sh
node taarof-web/scripts/checkpoint-xterm.mjs corpus.json xterm-results.json
```

Both commands fail on differences. The browser harness uses a private profile,
synthetic inert bytes and a loopback Vite server. It does not use the system
clipboard. Retain the exact source commit, built example hash, container image
identity and JSON results alongside a review receipt.

## Subscriber memory accounting

Limits apply per broker pane. Native queued payload is at most `4 × 4 MiB =
16 MiB`, plus one 8 KiB in-flight chunk per receiver and one 8 KiB reader chunk
(40 KiB total). Each queue has at most 512 entries; on the supported 64-bit
build, the `Vec<u8>` entry metadata is `4 × 512 × 24 = 49,152` bytes, in addition
to bounded queue/lock/weak-handle bookkeeping and allocator overhead. Initial
replay is packed directly into those queues, without another seed vector. The
existing replay window separately retains at most 4 MiB of payload and its
frame metadata. Queue byte and entry limits both apply; many smaller reads can
hit the entry limit before 4 MiB.

The sixteen web observers retain no queued output payload. Their active replay
batches together retain at most `16 × 64 KiB = 1 MiB` plus eight frame records
per observer. Each connection can instead retain a checkpoint with at most
256 KiB ANSI, base64 expansion (at most 349,528 bytes), JSON construction and a
serialized pending message. Encoding can temporarily coexist with copies made
by JSON conversion; count the ANSI and up to three expanded text copies, plus
frame-context strings, JSON node metadata and allocation capacity. A checkpoint
is therefore not a 256 KiB total allocation promise.

Checkpoint hashing also constructs visible text, material/attributed cell
projections, JSON values, sorted serialization and hash padding. The admission
estimate bounds the grid and text before any of these copies; it includes a
second copy of cell text for cursor restoration. Only one checkpoint builds
these model/hash intermediates under a pane's lock at a time. The number of
rows/cells and all copied text are bounded by the admitted estimate, but their
object metadata, serialization expansion and allocator capacity must still be
counted when measuring memory. The estimate rejects oversized grids immediately.

## Combining-text retention and recovery

Owner-approved policy (2026-09-05): each model cell retains at most **1,024 UTF-8
bytes**, including the base glyph. A combining mark is appended only when checked
addition proves the complete code point fits. Otherwise the retained prefix stays
in place and the cell becomes **degraded**. Later combining marks allocate no more
cell storage. There is no automatic terminal clear or reset.

Degradation belongs to the cell and survives buffer switches and copies. While
**either** primary or alternate buffer contains a degraded cell, all public model
projection, hash and checkpoint methods return `ModelDegraded` before cloning text
or serializing it. `checkpoint_fits` returns false. The broker returns a typed
error, and web checkpoint attempts send `checkpoint_degraded` with explicit
recovery instructions and close the attach. They never send a plausible but
truncated checkpoint. Web clients with continuous exact replay can continue;
a client requiring reconstruction must retry after recovery.

Overwriting or erasing the affected cell releases its incomplete text and clears
its degraded flag. Overwriting either half of a wide glyph erases the whole glyph.
Scrolling/cropping that removes a cell is an erasure within this model's existing
visible-buffer semantics; it does not claim scrollback fidelity. Entering a fresh
alternate buffer erases that buffer, but cannot clear degradation in the primary.
Erasing only the active buffer cannot clear a hidden degraded buffer. A terminal
RIS (`ESC c`) resets both buffers. These are ordinary explicit terminal operations,
not injected recovery bytes. Unrelated output does not restore checkpoint truth.

Native presentation and bounded replay retain the **original byte stream**. The
model cap never edits PTY bytes, input, sequence counters, or native/replay delivery.
Native VTE and xterm may retain combining text differently; equality is claimed
only for the supported differential corpus and tested post-overwrite/erase/reset
recovery, not arbitrary overflowed content.

For fixed dimensions, retained cell-text payload is at most
`2 × rows × cols × 1,024` bytes, with separate `String`/cell/grid metadata and allocator
overhead. Per-cell reservations do not geometrically grow beyond the cap;
`retained_text_bytes` uses allocation-free checked summation. Projection and hash
copies are refused before allocation when degraded; healthy copies are bounded by
the fixed grid and cap. Observer checkpoints additionally retain their existing
256 KiB pre-allocation budget with checked arithmetic. This is not an app-wide RSS
cap: other buffers, parser framing, renderer memory and caller-provided input have
separate contracts.

Reproduce the retention and exact native/replay delivery checks:

```sh
cargo test --manifest-path taarof-app/Cargo.toml combining -- --nocapture
cargo test --manifest-path taarof-app/Cargo.toml pty_broker
# In the isolated Kasm display, after building the renderer example:
checkpoint_renderer_probe --combining-retention > combining-corpus.json
node taarof-web/scripts/checkpoint-xterm.mjs combining-corpus.json combining-xterm.json
```

The retained-byte probe uses bounded-cell inert output and
reports queue bytes/counts, replay bytes, producer progress, byte digest and RSS
as distinct evidence:

```sh
cargo run --manifest-path taarof-app/Cargo.toml --example subscriber_memory_probe -- 32
cargo run --manifest-path taarof-app/Cargo.toml --example subscriber_memory_probe -- 64
```
