// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

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

fn pull_cue(h: &mut gst_check::Harness) -> (gst::ClockTime, gst::ClockTime, String) {
    let buf = h.pull().expect("cue");
    let pts = buf.pts().unwrap();
    let dur = buf.duration().unwrap();
    let map = buf.map_readable().unwrap();
    let text = std::str::from_utf8(map.as_slice()).unwrap().to_string();
    (pts, dur, text)
}

#[test]
fn single_word_eos_emits_pending() {
    init();
    let mut h = gst_check::Harness::new("textrollup");
    {
        let el = h.element().unwrap();
        el.set_property("columns", 42u32);
        el.set_property("lines", 2u32);
        el.set_property("hold", 250u32);
        el.set_property("clear-timeout", 0u32);
    }
    h.set_src_caps_str("text/x-raw, format=utf8");

    push_word(&mut h, 1000, 100, "Hello");
    // No cue yet — held pending.
    assert!(h.try_pull().is_none());

    assert!(h.push_event(gst::event::Eos::new()));
    let (pts, dur, text) = pull_cue(&mut h);
    assert_eq!(pts, ms(1000));
    assert_eq!(dur, ms(250));
    assert_eq!(text, "Hello");
}

#[test]
fn next_word_closes_previous_exactly() {
    init();
    let mut h = gst_check::Harness::new("textrollup");
    {
        let el = h.element().unwrap();
        el.set_property("hold", 250u32);
        el.set_property("clear-timeout", 0u32);
        el.set_property("break-on-sentence", false);
    }
    h.set_src_caps_str("text/x-raw, format=utf8");

    push_word(&mut h, 1000, 80, "One");
    push_word(&mut h, 1200, 80, "two");

    let (pts, dur, text) = pull_cue(&mut h);
    assert_eq!(pts, ms(1000));
    assert_eq!(dur, ms(200));
    assert_eq!(text, "One");

    assert!(h.push_event(gst::event::Eos::new()));
    let (pts, dur, text) = pull_cue(&mut h);
    assert_eq!(pts, ms(1200));
    assert_eq!(dur, ms(250));
    assert_eq!(text, "One two");
}

#[test]
fn overlong_word_is_its_own_line() {
    init();
    let mut h = gst_check::Harness::new("textrollup");
    {
        let el = h.element().unwrap();
        el.set_property("columns", 5u32);
        el.set_property("lines", 2u32);
        el.set_property("hold", 100u32);
        el.set_property("clear-timeout", 0u32);
        el.set_property("break-on-sentence", false);
    }
    h.set_src_caps_str("text/x-raw, format=utf8");

    push_word(&mut h, 0, 50, "toolong");
    push_word(&mut h, 100, 50, "ok");
    assert!(h.push_event(gst::event::Eos::new()));

    let (_pts, _dur, first) = pull_cue(&mut h);
    assert_eq!(first, "toolong");
    let (_pts, _dur, second) = pull_cue(&mut h);
    assert_eq!(second, "toolong\nok");
}

#[test]
fn gap_hold_and_persist_then_clear() {
    init();
    let mut h = gst_check::Harness::new("textrollup");
    {
        let el = h.element().unwrap();
        el.set_property("hold", 200u32);
        el.set_property("persist", 500u32);
        el.set_property("clear-timeout", 1500u32);
        el.set_property("break-on-sentence", false);
    }
    h.set_src_caps_str("text/x-raw, format=utf8");

    push_word(&mut h, 1000, 50, "Hi");
    // Frontier to 1000+200 = hold close+reopen
    assert!(h.push_event(gst::event::Gap::builder(ms(1000)).duration(ms(200)).build()));
    let (pts, dur, text) = pull_cue(&mut h);
    assert_eq!(pts, ms(1000));
    assert_eq!(dur, ms(200));
    assert_eq!(text, "Hi");

    // Persist chunk: pending now at 1200, persist 500 → emit at 1700
    assert!(h.push_event(gst::event::Gap::builder(ms(1200)).duration(ms(500)).build()));
    let (pts, dur, text) = pull_cue(&mut h);
    assert_eq!(pts, ms(1200));
    assert_eq!(dur, ms(500));
    assert_eq!(text, "Hi");

    // Clear at silence_since(1000)+1500=2500
    assert!(h.push_event(gst::event::Gap::builder(ms(1700)).duration(ms(900)).build()));
    let (pts, dur, text) = pull_cue(&mut h);
    assert_eq!(pts, ms(1700));
    assert_eq!(dur, ms(800)); // until clear at 2500
    assert_eq!(text, "Hi");

    // After clear, further gaps produce no more cues.
    assert!(h.try_pull().is_none());
}

#[test]
fn flush_stop_drops_pending() {
    init();
    let mut h = gst_check::Harness::new("textrollup");
    h.set_src_caps_str("text/x-raw, format=utf8");

    push_word(&mut h, 1000, 50, "Hello");
    assert!(h.push_event(gst::event::FlushStart::new()));
    assert!(h.push_event(gst::event::FlushStop::new(true)));
    assert!(h.push_event(gst::event::Eos::new()));
    assert!(h.try_pull().is_none());
}

#[test]
fn latency_query_adds_hold() {
    init();
    let mut h = gst_check::Harness::new_parse("identity ! textrollup hold=250 name=r");
    h.set_src_caps_str("text/x-raw, format=utf8");
    h.play();

    let el = h.element().unwrap();
    let src = el.static_pad("src").unwrap();
    let mut q = gst::query::Latency::new();
    assert!(src.query(&mut q));
    let (_live, min, _max) = q.result();
    assert!(min >= ms(250), "min latency {min} should include hold");
}

#[test]
fn empty_stream_eos() {
    init();
    let mut h = gst_check::Harness::new("textrollup");
    h.set_src_caps_str("text/x-raw, format=utf8");
    assert!(h.push_event(gst::event::Eos::new()));
    assert!(h.try_pull().is_none());
}

#[test]
fn hold_change_posts_latency_message() {
    init();

    let pipeline = gst::Pipeline::default();
    let el = gst::ElementFactory::make("textrollup")
        .name("r")
        .build()
        .unwrap();
    pipeline.add(&el).unwrap();
    pipeline.set_state(gst::State::Paused).unwrap();

    let bus = pipeline.bus().expect("pipeline bus");
    // Drain any startup messages.
    while bus.pop().is_some() {}

    el.set_property("hold", 500u32);

    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(1),
        &[gst::MessageType::Latency],
    );
    assert!(msg.is_some(), "expected LATENCY message after hold change");
    let _ = pipeline.set_state(gst::State::Null);
}
