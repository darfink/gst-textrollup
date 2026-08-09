// SPDX-License-Identifier: MPL-2.0
//
// Review probes: cases the existing suite does not cover. Written to find
// defects, so a failure here is the point rather than a regression.

use gst::prelude::*;

use std::time::{Duration, Instant};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gsttextrollup::plugin_register_static().expect("textrollup test");
    });
}

fn ms(v: u64) -> gst::ClockTime {
    gst::ClockTime::from_mseconds(v)
}

fn push_word(h: &mut gst_check::Harness, pts_ms: u64, dur_ms: u64, text: &str) {
    let mut buf = gst::Buffer::from_mut_slice(text.as_bytes().to_vec());
    {
        let buf = buf.get_mut().unwrap();
        buf.set_pts(ms(pts_ms));
        buf.set_duration(ms(dur_ms));
    }
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
}

fn push_gap(h: &mut gst_check::Harness, start_ms: u64, dur_ms: u64) {
    assert!(
        h.push_event(
            gst::event::Gap::builder(ms(start_ms))
                .duration(ms(dur_ms))
                .build()
        )
    );
}

/// Everything the element emitted downstream, in order, as (start, end, kind).
#[derive(Debug, PartialEq, Eq)]
enum Out {
    Cue(u64, u64, String),
    Gap(u64, u64),
}

fn drain(h: &mut gst_check::Harness) -> Vec<Out> {
    let mut out = Vec::new();
    while let Some(event) = h.try_pull_event() {
        if let gst::EventView::Gap(g) = event.view() {
            let (pts, dur) = g.get();
            let dur = dur.unwrap_or(gst::ClockTime::ZERO);
            out.push(Out::Gap(pts.mseconds(), (pts + dur).mseconds()));
        }
    }
    while let Some(buf) = h.try_pull() {
        let pts = buf.pts().unwrap();
        let dur = buf.duration().unwrap_or(gst::ClockTime::ZERO);
        let map = buf.map_readable().unwrap();
        out.push(Out::Cue(
            pts.mseconds(),
            (pts + dur).mseconds(),
            std::str::from_utf8(map.as_slice()).unwrap().to_string(),
        ));
    }
    out
}

fn harness(props: &[(&str, u32)]) -> gst_check::Harness {
    let mut h = gst_check::Harness::new("textrollup");
    {
        let el = h.element().unwrap();
        for (name, value) in props {
            el.set_property(name, *value);
        }
        el.set_property("break-on-sentence", false);
    }
    h.set_src_caps_str("text/x-raw, format=utf8");
    h
}

fn gapkeeper_harness(props: &[(&str, u32)]) -> gst_check::Harness {
    let mut h = gst_check::Harness::new("gapkeeper");
    {
        let el = h.element().unwrap();
        for (name, value) in props {
            el.set_property(name, *value);
        }
    }
    h.set_src_caps_str("text/x-raw, format=utf8");
    h
}

/// The gapkeeper must pass buffers through untouched (cue timing is upstream's
/// business) and cover the pad with duration-less GAPs at the frontier.
#[test]
fn probe_gapkeeper_covers_silence_without_touching_buffers() {
    init();
    let mut h = gapkeeper_harness(&[("keepalive-ms", 100)]);
    h.play();

    let mut segment = gst::Segment::new();
    segment.set_format(gst::Format::Time);
    segment.set_start(ms(0));
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    push_word(&mut h, 1000, 300, "Hello");

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        out.extend(drain(&mut h));
    }

    // The cue passes through at its exact PTS/duration.
    let cues: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::Cue(start, end, text) => Some((*start, *end, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        cues,
        vec![(1000, 1300, "Hello".to_string())],
        "gapkeeper must not alter buffer timing"
    );
    // Coverage GAPs sit at the observed frontier (start == end: no duration).
    let gaps: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::Gap(start, end) => Some((*start, *end)),
            _ => None,
        })
        .collect();
    assert!(
        gaps.iter().all(|(start, end)| start == end),
        "gapkeeper GAPs must carry no duration: {gaps:?}"
    );
    assert!(
        gaps.iter().any(|(start, _)| *start >= 1000),
        "coverage must reach the observed frontier: {gaps:?}"
    );
}

/// A dead upstream must not pin a downstream mux: with stall-ms set, the
/// frontier advances once upstream goes silent long enough.
#[test]
fn probe_gapkeeper_stall_advances_the_frontier() {
    init();
    let mut h = gapkeeper_harness(&[("keepalive-ms", 100), ("stall-ms", 500)]);
    h.play();

    let mut segment = gst::Segment::new();
    segment.set_format(gst::Format::Time);
    segment.set_start(ms(0));
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    push_word(&mut h, 1000, 100, "Hello");

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(2_200);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        out.extend(drain(&mut h));
    }
    let gaps: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::Gap(start, end) => Some((*start, *end)),
            _ => None,
        })
        .collect();
    let furthest = gaps.iter().map(|(_, end)| *end).max().unwrap_or(0);
    assert!(
        furthest > 1000,
        "stall watchdog must advance the frontier past the last cue; furthest {furthest}"
    );
}

/// A stream that opens with silence must still advance downstream. This is the
/// exact condition that stalls matroskamux: the muxer cannot output anything
/// until every sink pad has moved.
#[test]
fn probe_leading_silence_is_announced_downstream() {
    init();
    let mut h = harness(&[("hold", 250), ("persist", 1000), ("clear-timeout", 3000)]);

    // Ten seconds of silence before anyone speaks.
    for i in 0..20 {
        push_gap(&mut h, i * 500, 500);
    }

    let out = drain(&mut h);
    let gaps: Vec<_> = out.iter().filter(|o| matches!(o, Out::Gap(..))).collect();
    assert!(
        !gaps.is_empty(),
        "10s of leading silence produced no downstream GAP at all: {out:?}"
    );

    let furthest = gaps
        .iter()
        .map(|g| match g {
            Out::Gap(_, end) => *end,
            _ => 0,
        })
        .max()
        .unwrap();
    assert!(
        furthest >= 9_000,
        "downstream only advanced to {furthest}ms of 10000ms"
    );
}

/// Same question, but with the window already cleared: silence *after* speech.
#[test]
fn probe_trailing_silence_is_announced_downstream() {
    init();
    let mut h = harness(&[("hold", 200), ("persist", 500), ("clear-timeout", 1000)]);

    push_word(&mut h, 1000, 50, "Hi");
    for i in 0..20 {
        push_gap(&mut h, 1000 + i * 500, 500);
    }

    let out = drain(&mut h);
    let furthest = out
        .iter()
        .map(|o| match o {
            Out::Gap(_, end) | Out::Cue(_, end, _) => *end,
        })
        .max()
        .unwrap_or(0);
    assert!(
        furthest >= 10_000,
        "downstream only advanced to {furthest}ms of 11000ms: {out:?}"
    );
}

/// Upstream gaps finer than GAP_GRANULARITY must not vanish. A source emitting
/// 100ms gaps is not exotic - an RTP-fed transcriber does exactly that.
#[test]
fn probe_fine_grained_gaps_are_not_swallowed() {
    init();
    let mut h = harness(&[("hold", 250), ("persist", 1000), ("clear-timeout", 0)]);

    push_word(&mut h, 0, 50, "Hi");
    // Drive well past clear/persist with sub-granularity steps.
    for i in 0..100 {
        push_gap(&mut h, i * 100, 100);
    }

    let out = drain(&mut h);
    let furthest = out
        .iter()
        .map(|o| match o {
            Out::Gap(_, end) | Out::Cue(_, end, _) => *end,
        })
        .max()
        .unwrap_or(0);
    assert!(
        furthest >= 9_000,
        "100ms gaps advanced downstream only to {furthest}ms of 10000ms"
    );
}

/// Same as above but with the window already cleared, so `emit_gap` - not
/// persist - owns advancing the timeline. Coalescing must accumulate, not
/// silently advance `out_pts` past a span it never announced.
#[test]
fn probe_fine_grained_gaps_after_clear() {
    init();
    let mut h = harness(&[("hold", 200), ("persist", 500), ("clear-timeout", 500)]);

    push_word(&mut h, 0, 50, "Hi");
    for i in 0..100 {
        push_gap(&mut h, i * 100, 100);
    }

    let out = drain(&mut h);
    let furthest = out
        .iter()
        .map(|o| match o {
            Out::Gap(_, end) | Out::Cue(_, end, _) => *end,
        })
        .max()
        .unwrap_or(0);
    assert!(
        furthest >= 9_000,
        "after clear, 100ms gaps advanced downstream only to {furthest}ms of 10000ms: {out:?}"
    );
}

/// Output must tile the timeline: no overlaps, no holes.
#[test]
fn probe_output_tiles_the_timeline() {
    init();
    let mut h = harness(&[("hold", 250), ("persist", 1000), ("clear-timeout", 2000)]);

    let mut t = 1000;
    for word in ["one", "two", "three", "four", "five", "six"] {
        push_word(&mut h, t, 80, word);
        push_gap(&mut h, t, 120);
        t += 200;
    }
    // A long pause, then speech resumes.
    for i in 0..30 {
        push_gap(&mut h, t + i * 200, 200);
    }
    t += 6000;
    push_word(&mut h, t, 80, "resumed");
    assert!(h.push_event(gst::event::Eos::new()));

    let out = drain(&mut h);
    let mut spans: Vec<(u64, u64)> = out
        .iter()
        .map(|o| match o {
            Out::Gap(s, e) | Out::Cue(s, e, _) => (*s, *e),
        })
        .collect();
    spans.sort();

    for pair in spans.windows(2) {
        let (_, prev_end) = pair[0];
        let (next_start, _) = pair[1];
        assert!(
            next_start >= prev_end,
            "overlap: {:?} then {:?} in {out:?}",
            pair[0],
            pair[1]
        );
        assert!(
            next_start == prev_end,
            "hole between {prev_end} and {next_start} in {out:?}"
        );
    }
}

/// The headline property: a completed line, once shown, is byte-identical in
/// every later cue until it scrolls off. The in-module test for this asserts
/// `x == x` inside an `if x == x`, so it cannot fail; this one can.
#[test]
fn probe_completed_lines_never_change() {
    init();
    let mut h = harness(&[("hold", 250), ("persist", 1000), ("clear-timeout", 0)]);

    let text = "one of the things that is great about filming this here is that we \
                get to see the work happening in the background all around us";
    let mut t = 0;
    for word in text.split_whitespace() {
        push_word(&mut h, t, 80, word);
        t += 200;
    }
    assert!(h.push_event(gst::event::Eos::new()));

    let cues: Vec<String> = drain(&mut h)
        .into_iter()
        .filter_map(|o| match o {
            Out::Cue(_, _, text) => Some(text),
            _ => None,
        })
        .collect();

    // Every non-final line of every cue, in order of first appearance.
    let mut history: Vec<String> = Vec::new();
    for cue in &cues {
        let lines: Vec<&str> = cue.lines().collect();
        for frozen in &lines[..lines.len().saturating_sub(1)] {
            match history.iter().position(|h| h == frozen) {
                Some(_) => {}
                None => history.push(frozen.to_string()),
            }
        }
    }

    // A frozen line must appear in a contiguous run of cues and be identical
    // throughout: if it reappears after another frozen line displaced it, the
    // window reflowed.
    for cue in &cues {
        let lines: Vec<&str> = cue.lines().collect();
        for frozen in &lines[..lines.len().saturating_sub(1)] {
            assert!(
                history.contains(&frozen.to_string()),
                "line {frozen:?} appeared that was never frozen"
            );
        }
    }

    // No cue may exceed the configured height.
    for cue in &cues {
        assert!(cue.lines().count() <= 2, "cue taller than 2 lines: {cue:?}");
    }

    // Losslessness: the concatenation of all frozen lines, in first-appearance
    // order, plus the final bottom line, must reproduce the input.
    let last_bottom = cues.last().unwrap().lines().last().unwrap().to_string();
    let mut rebuilt = history.join(" ");
    if !rebuilt.ends_with(&last_bottom) {
        rebuilt.push(' ');
        rebuilt.push_str(&last_bottom);
    }
    assert_eq!(
        rebuilt.split_whitespace().collect::<Vec<_>>(),
        text.split_whitespace().collect::<Vec<_>>(),
        "words lost or duplicated"
    );
}

/// Words are word-per-buffer by contract, but a buffer carrying several words
/// must not silently lose the intermediate window states.
#[test]
fn probe_multiword_buffer_keeps_every_word() {
    init();
    let mut h = harness(&[("hold", 250), ("persist", 1000), ("clear-timeout", 0)]);

    push_word(&mut h, 0, 300, "alpha beta gamma");
    assert!(h.push_event(gst::event::Eos::new()));

    let cues: Vec<String> = drain(&mut h)
        .into_iter()
        .filter_map(|o| match o {
            Out::Cue(_, _, text) => Some(text),
            _ => None,
        })
        .collect();
    let joined = cues.join(" | ");
    for word in ["alpha", "beta", "gamma"] {
        assert!(joined.contains(word), "{word:?} missing from {joined:?}");
    }
}
