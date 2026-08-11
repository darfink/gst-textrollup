# gst-textrollup

A GStreamer text filter that turns timestamped word-level text into a fixed
roll-up caption window. Completed lines freeze; only the bottom line grows:

```text
transcribecpptranscriber ! textrollup ! fakesink
```

The element is `textrollup`; the plugin is `textrollup`. It is designed to sit
after a speech-to-text element such as
[gst-transcribe-cpp](https://github.com/darfink/gst-transcribe-cpp), or after
any source that emits timestamped `text/x-raw,format=utf8` buffers.

It is built for live captioning, where the viewer is reading the text while it
is still being written. Two properties follow from that, and between them they
account for most of the design:

- **Nothing already on screen moves.** A line's text is decided once, when it
  fills, and is never recomputed.
- **A cue is emitted the moment a word commits**, rather than batched toward a
  sentence boundary, so the only latency the element adds is its `hold`.

In the position it is meant for it replaces `textaccumulate` + `textwrap`,
because owning the line breaking is precisely what makes the first property
possible.

## How cues are built up

One buffer in, one word. One buffer out, the whole visible window. Feeding it
`one of the things that's great about filming this here is that we get to see
the work happening` at `columns=42 lines=2`, one word every 240 ms:

```text
00:00.000 --> 00:00.240   one
00:00.240 --> 00:00.480   one of
00:00.480 --> 00:00.720   one of the
00:00.720 --> 00:00.960   one of the things
00:00.960 --> 00:01.200   one of the things that's
00:01.200 --> 00:01.440   one of the things that's great
00:01.440 --> 00:01.680   one of the things that's great about     <- line is full
00:01.680 --> 00:01.920   one of the things that's great about     <- frozen from
                          filming                                     here on
00:01.920 --> 00:02.160   one of the things that's great about
                          filming this
             (this here is that we get to, one word per cue)
00:03.600 --> 00:03.840   one of the things that's great about
                          filming this here is that we get to see
00:03.840 --> 00:04.080   filming this here is that we get to see  <- scrolled up,
                          the                                         unchanged
00:04.080 --> 00:04.320   filming this here is that we get to see
                          the work
00:04.320 --> 00:04.570   filming this here is that we get to see
                          the work happening
```

`one of the things that's great about` is decided at 00:01.440 and is
byte-identical in every later cue until it scrolls off at 00:03.840.

Each cue carries the **entire** window and ends exactly where the next one
begins, so exactly one cue is ever on screen. Overlapping cues would be the
other way to express a scrolling window, but players stack those rather than
replacing them, which defeats the purpose.

### Why not wrap the text instead

Because wrapping is a global decision and roll-up needs a local one. Once the
visible text fills the window, re-wrapping means re-wrapping a *sliding*
window, and the line breaks move. The same words, re-wrapped on each new word:

```text
was:  of the things that's great, Satya, about
now:  that's great, Satya, about filming this
```

`filming this` has jumped from the bottom line up onto the line above. That
happens several times a second and reads as the whole caption twitching.
`textrollup` never re-wraps, so it cannot happen.

## Build

Requires Rust 1.92+ and GStreamer 1.20 development headers.

```bash
git clone https://github.com/darfink/gst-textrollup
cd gst-textrollup
cargo build --release
```

Expose the plugin to GStreamer and confirm that it registered:

```bash
export GST_PLUGIN_PATH="$PWD/target/release"
gst-inspect-1.0 textrollup
```

To install it permanently, copy
`target/release/libgsttextrollup.so` (`.dylib` on macOS) into a GStreamer plugin
directory.

## Try it

The most useful smoke test combines it with
[gst-transcribe-cpp](https://github.com/darfink/gst-transcribe-cpp) and a GGUF
model. After installing or exposing both plugins to GStreamer, run:

```bash
gst-launch-1.0 filesrc location=speech.wav ! decodebin ! audioconvert ! audioresample \
  ! transcribecpptranscriber mode=stream backend=cpu \
      model-path=nemotron-speech-streaming-en-0.6b-Q8_0.gguf \
  ! textrollup columns=42 lines=2 hold=250 \
  ! fakesink dump=true
```

The transcriber supplies one committed word per timestamped buffer and GAP
events during silence. `textrollup` emits complete rendered windows as text
buffers, so the dump contains captions such as:

```text
one of the things
that is great
```

The output buffer carries the window text, its media PTS, and its duration.
Timeline holes are represented by downstream GAP events.

### Docker

The image builds and registers the filter without requiring a local Rust or
GStreamer development toolchain:

```bash
docker build -t gst-textrollup .
docker run --rm gst-textrollup
```

The default command runs `gst-inspect-1.0 textrollup`. The image contains this
filter only; speech recognition, audio fixtures, and models are intentionally
left to the companion transcriber image or to a host pipeline.

## Window behaviour

A line is completed either by filling to `columns` or by sentence-final
punctuation when `break-on-sentence` is set, and is fixed byte-for-byte from
that moment until it scrolls out of the window. Width is Unicode display width,
so wide CJK characters count as two columns, and a single word longer than
`columns` gets a line to itself rather than being broken mid-word.

`columns`, `lines` and `break-on-sentence` are writable while playing, but they
affect future wrapping only: lines that are already frozen keep the geometry
they were built with, since un-freezing them is the one thing this element
exists to avoid.

The element accepts word-level buffers. If a buffer contains multiple words,
they are split at whitespace and share that buffer's PTS as a degraded-input
fallback. Buffers must have a PTS and valid UTF-8.

## Silence and timing

The element uses media-time GAP events rather than a wall-clock timer:

| Property | Default | Meaning |
| --- | ---: | --- |
| `hold` | 250 ms | Maximum wait for a successor word before closing the current cue; contributes to latency |
| `persist` | 1000 ms | Duration of each identical re-emitted cue during silence |
| `clear-timeout` | 3000 ms | Silence after which the window clears; `0` never clears |

After `hold`, the current caption is closed and reopened. During a longer
silence, `persist` keeps the display continuous with identical caption buffers.
Once `clear-timeout` is reached, the window is cleared and the remaining
timeline is emitted as GAP events.

Upstream should announce silence with GAP events. Without them, the element can
still close cues when later words arrive, but it cannot advance persistence or
clear-timeout during an otherwise silent stream. A diagnostic warning is emitted
after 30 seconds of timestamped speech without a GAP event.

## Properties

| Property | Default | Meaning |
| --- | ---: | --- |
| `columns` | 42 | Maximum display width per line |
| `lines` | 2 | Number of lines in the roll-up window |
| `hold` | 250 ms | Holdback before closing a cue; minimum 1 ms |
| `persist` | 1000 ms | Duration of each silence re-emission; minimum 1 ms |
| `clear-timeout` | 3000 ms | Silence before clearing; `0` disables clearing |
| `break-on-sentence` | `true` | Finish the current line at `.`, `!`, `?`, or `…`, including trailing quotes/brackets |
| `emit-clear-cue` | `false` | Also emit an empty cue at `clear-timeout`, announcing that the display is blank |

### Announcing the clear

`clear-timeout` decides when the window comes down, but a GAP only moves the
frontier — it says nothing about the display. That is enough for a transport
whose cues carry their own end, and not enough for one where a cue shows until
something replaces it, such as FLV script data (`onCaption`/`onTextData`).

With `emit-clear-cue`, the clear is stated on the cue timeline as a
zero-duration empty cue at the clear position. A consumer that reads an empty
cue as "stop displaying" then ends the caption exactly where this element
decided, instead of inventing an end from a timeout of its own — which is what
otherwise happens, and which makes the caption disappear at the consumer's
chosen time rather than the publisher's.

It is off by default because an empty cue is meaningless to a consumer that
does not implement it: such a consumer will show an empty caption, or reject
the cue outright, since a blank body terminates a WebVTT cue.

All properties are readable and writable through the PAUSED and PLAYING states.

## Integration contract

The sink and source pads both carry:

```text
text/x-raw, format=utf8
```

Every input buffer must have a timestamp. The element forwards stream-start,
segment, flush, and relevant custom downstream events. A new segment clears the
caption window so text from the previous timeline cannot leak into the new one.

The element reports its `hold` value as additional downstream latency and posts
a LATENCY message when that property changes.

## Debugging

```bash
GST_DEBUG=textrollup:6 gst-launch-1.0 ...
```

Logs include timeline anchoring, GAP handling, cue ranges, rendered text, and
diagnostics for missing timestamps or missing upstream GAP events.

## Tests

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The suite covers the pure roll-up window, a fixture oracle, GStreamer holdback,
GAP persistence and clearing, EOS, flush, latency, leading/trailing silence,
fine-grained GAPs, multi-word buffers, timeline tiling, and fixation of
completed lines.

## Status and limitations

The word-window and GStreamer state machine are covered by unit and harness
tests. The filter has also been exercised after `gst-transcribe-cpp` on speech
and speech/silence/speech fixtures.

The timing tests are hand-written invariant probes rather than a complete
reference oracle for every combination of words, GAPs, seeks, and dynamic
property changes. In particular, upstream timestamp/GAP quality remains part
of the integration contract.

## License

MPL-2.0.
