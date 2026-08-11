// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::LazyLock;
use std::sync::Mutex;

use super::window::{Push, Window};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "textrollup",
        gst::DebugColorFlags::empty(),
        Some("Roll-up caption window"),
    )
});

const DEFAULT_COLUMNS: u32 = 42;
const DEFAULT_LINES: u32 = 2;
const DEFAULT_HOLD_MS: u32 = 250;
const DEFAULT_PERSIST_MS: u32 = 1000;
const DEFAULT_CLEAR_TIMEOUT_MS: u32 = 3000;

/// Whether clearing the window also emits an empty cue.
///
/// Off by default: an empty cue is meaningful only to a downstream that reads
/// one as "stop displaying", and formats that carry their own cue end have no
/// use for it. Enabling it where it is not understood risks an empty caption
/// rather than a cleared one.
const DEFAULT_EMIT_CLEAR_CUE: bool = false;
const DEFAULT_BREAK_ON_SENTENCE: bool = true;

/// Smallest advance worth a gap event. Matches the transcriber's coalescing.
const GAP_GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(200);

/// How much speech may pass with no GAP event at all before we complain.
///
/// A conforming transcriber only emits gaps when its frontier outruns the words
/// it has published, so continuous speech legitimately produces none. Half a
/// minute of it does not: that is upstream not implementing the convention, and
/// the visible symptom - captions that never clear - is otherwise a puzzle.
const WARN_NO_GAPS_AFTER: gst::ClockTime = gst::ClockTime::from_seconds(30);

#[derive(Debug, Clone)]
struct Settings {
    columns: u32,
    lines: u32,
    hold_ms: u32,
    persist_ms: u32,
    clear_timeout_ms: u32,
    emit_clear_cue: bool,
    break_on_sentence: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            columns: DEFAULT_COLUMNS,
            lines: DEFAULT_LINES,
            hold_ms: DEFAULT_HOLD_MS,
            persist_ms: DEFAULT_PERSIST_MS,
            clear_timeout_ms: DEFAULT_CLEAR_TIMEOUT_MS,
            emit_clear_cue: DEFAULT_EMIT_CLEAR_CUE,
            break_on_sentence: DEFAULT_BREAK_ON_SENTENCE,
        }
    }
}

#[derive(Debug, Clone)]
struct Pending {
    start: gst::ClockTime,
    text: String,
    /// Whether hold rule (b) has already closed+reopened this pending cue.
    /// Prevents re-arming hold on every reopen; persist takes over after that.
    hold_applied: bool,
}

struct State {
    window: Window,
    pending: Option<Pending>,
    /// End of the last buffer or gap pushed on the src pad.
    out_pts: Option<gst::ClockTime>,
    /// Furthest media-time frontier observed from upstream GAPs / words.
    frontier: Option<gst::ClockTime>,
    /// PTS of the most recent word. Clear-timeout is measured from here.
    last_word_pts: Option<gst::ClockTime>,
    /// PTS of the first word since the last reset, for the no-GAP diagnostic.
    first_word_pts: Option<gst::ClockTime>,
    /// Whether we already warned that upstream emits no GAP events.
    warned_no_gaps: bool,
    seen_gap: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            window: Window::new(
                DEFAULT_COLUMNS as usize,
                DEFAULT_LINES as usize,
                DEFAULT_BREAK_ON_SENTENCE,
            ),
            pending: None,
            out_pts: None,
            frontier: None,
            last_word_pts: None,
            first_word_pts: None,
            warned_no_gaps: false,
            seen_gap: false,
        }
    }
}

impl State {
    fn reset_all(&mut self, settings: &Settings) {
        let warned_no_gaps = self.warned_no_gaps;
        *self = State {
            window: Window::new(
                settings.columns.max(1) as usize,
                settings.lines.max(1) as usize,
                settings.break_on_sentence,
            ),
            warned_no_gaps,
            ..State::default()
        };
    }

    /// A new segment is a discontinuity: the words already on screen belong to
    /// the old timeline, so carrying them over would stamp stale text with
    /// fresh timestamps.
    fn reset_timeline(&mut self) {
        self.out_pts = None;
        self.frontier = None;
        self.last_word_pts = None;
        self.first_word_pts = None;
        self.pending = None;
        self.window.clear();
    }

    fn apply_settings(&mut self, settings: &Settings) {
        self.window.set_columns(settings.columns.max(1) as usize);
        self.window.set_lines(settings.lines.max(1) as usize);
        self.window
            .set_break_on_sentence(settings.break_on_sentence);
    }
}

/// Something to push downstream once the state lock has been released.
///
/// Pushing under the lock would let a blocked downstream queue hold it, and
/// anything else touching state - a property set from the application thread -
/// would stall behind unrelated backpressure.
#[derive(Debug)]
enum Out {
    Gap(gst::ClockTime, gst::ClockTime),
    Cue(gst::ClockTime, gst::ClockTime, String),
}

pub struct TextRollup {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl TextRollup {
    fn post_latency_changed(&self) {
        let obj = self.obj();
        gst::info!(
            CAT,
            imp = self,
            "latency changed, asking for a recalculation"
        );
        let _ = obj.post_message(gst::message::Latency::builder().src(&*obj).build());
    }

    /// Push everything queued while the lock was held, in order.
    fn flush(&self, out: Vec<Out>) -> Result<gst::FlowSuccess, gst::FlowError> {
        for item in out {
            match item {
                Out::Gap(start, end) => {
                    gst::trace!(CAT, imp = self, "gap at {start} for {}", end - start);
                    self.srcpad.push_event(
                        gst::event::Gap::builder(start)
                            .duration(end - start)
                            .build(),
                    );
                }
                Out::Cue(start, end, text) => {
                    gst::log!(
                        CAT,
                        imp = self,
                        "cue [{start}..{end}] ({}) {text:?}",
                        end - start
                    );
                    let mut buffer = gst::Buffer::from_mut_slice(text.into_bytes());
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(start);
                        buffer.set_duration(end - start);
                    }
                    self.srcpad.push(buffer)?;
                }
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn queue_cue(
        &self,
        state: &mut State,
        out: &mut Vec<Out>,
        start: gst::ClockTime,
        end: gst::ClockTime,
        text: String,
    ) {
        // Never emit behind what has already been published, or the cue
        // overlaps its predecessor and players stack the two.
        let start = state.out_pts.map_or(start, |out_pts| start.max(out_pts));
        if end <= start {
            gst::warning!(
                CAT,
                imp = self,
                "refusing non-positive cue [{start}..{end}] for {text:?}"
            );
            return;
        }

        if let Some(out_pts) = state.out_pts {
            if start > out_pts {
                out.push(Out::Gap(out_pts, start));
            }
        }

        state.out_pts = Some(end);
        out.push(Out::Cue(start, end, text));
    }

    /// Emit an empty cue announcing that the display is now blank.
    ///
    /// Zero-duration by construction: a clear is an instant on the timeline,
    /// not a span. It carries no text, and a consumer that understands it
    /// closes whatever cue is open and shows nothing in its place; one that
    /// does not will treat it as an empty caption, which is why this is opt-in.
    ///
    /// Deliberately not routed through [`Self::queue_cue`]: that refuses a
    /// non-positive span, because an ordinary cue with no duration is a bug.
    /// The frontier is left where it was, so the following GAP still covers
    /// the silence.
    fn queue_clear(&self, state: &mut State, out: &mut Vec<Out>, at: gst::ClockTime) {
        let at = state.out_pts.map_or(at, |out_pts| at.max(out_pts));
        gst::debug!(CAT, imp = self, "clearing the display at {at}");
        out.push(Out::Cue(at, at, String::new()));
    }

    /// Close the timeline up to `end`.
    ///
    /// The start is always the published frontier, so callers cannot open a
    /// hole. Does nothing until the timeline has an anchor - see
    /// [`Self::anchor_timeline`].
    fn queue_gap(&self, state: &mut State, out: &mut Vec<Out>, end: gst::ClockTime) {
        let Some(gap_start) = state.out_pts else {
            return;
        };
        if end <= gap_start {
            return;
        }

        // Coalesce advances too small to be worth an event. `out_pts` stays put
        // so the span keeps growing across calls - advancing it here would move
        // the start in lockstep with the end, the threshold would never be
        // reached, and the whole silence would be swallowed.
        if end < gap_start + GAP_GRANULARITY {
            return;
        }

        state.out_pts = Some(end);
        out.push(Out::Gap(gap_start, end));
    }

    /// Establish where our output timeline begins.
    ///
    /// Without this the first cue is the anchor, and every gap arriving before
    /// it is discarded - so a stream that opens with silence tells downstream
    /// nothing at all, and a muxer aggregating this pad has nothing to advance
    /// on until somebody finally speaks.
    fn anchor_timeline(&self, state: &mut State, pts: gst::ClockTime) {
        if state.out_pts.is_none() {
            gst::debug!(CAT, imp = self, "anchoring output timeline at {pts}");
            state.out_pts = Some(pts);
        }
    }

    fn close_pending_at(
        &self,
        state: &mut State,
        out: &mut Vec<Out>,
        end: gst::ClockTime,
        reopen: bool,
    ) {
        let Some(pending) = state.pending.take() else {
            return;
        };

        // Nothing to emit yet, and the cue must not be re-anchored backwards.
        if end <= pending.start {
            state.pending = Some(pending);
            return;
        }

        let text = pending.text.clone();
        self.queue_cue(state, out, pending.start, end, text);
        if reopen {
            state.pending = Some(Pending {
                start: end,
                text: pending.text,
                hold_applied: pending.hold_applied,
            });
        }
    }

    fn handle_word(&self, state: &mut State, out: &mut Vec<Out>, pts: gst::ClockTime, word: &str) {
        if word.is_empty() {
            return;
        }

        state.first_word_pts.get_or_insert(pts);
        state.last_word_pts = Some(pts);
        state.frontier = Some(state.frontier.map_or(pts, |frontier| frontier.max(pts)));

        if !state.seen_gap && !state.warned_no_gaps {
            if let Some(first) = state.first_word_pts {
                if pts >= first + WARN_NO_GAPS_AFTER {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "no GAP event in {WARN_NO_GAPS_AFTER} of speech; upstream does \
                         not announce its frontier, so captions will not clear on silence"
                    );
                    state.warned_no_gaps = true;
                }
            }
        }

        // Rule (a): next word arrives → close the previous cue exactly here.
        self.close_pending_at(state, out, pts, false);

        if let Push::Redraw(text) = state.window.push_word(word) {
            state.pending = Some(Pending {
                start: pts,
                text,
                hold_applied: false,
            });
        }
    }

    /// Advance media-time frontier. Drives hold (b), persist re-emission, and clear.
    fn advance_frontier(
        &self,
        state: &mut State,
        settings: &Settings,
        out: &mut Vec<Out>,
        frontier: gst::ClockTime,
    ) {
        if let Some(previous) = state.frontier {
            if frontier < previous {
                gst::warning!(
                    CAT,
                    imp = self,
                    "frontier moved backwards from {previous} to {frontier}"
                );
                return;
            }
        }
        state.frontier = Some(frontier);

        // Hold rule (b): close+reopen when the frontier reaches start+hold.
        // Only once per pending cue; after that persist owns silence re-emission.
        let hold_deadline = state.pending.as_ref().and_then(|pending| {
            (!pending.hold_applied)
                .then(|| pending.start + gst::ClockTime::from_mseconds(settings.hold_ms as u64))
        });
        if let Some(hold_deadline) = hold_deadline {
            if frontier >= hold_deadline {
                self.close_pending_at(state, out, hold_deadline, true);
                if let Some(pending) = state.pending.as_mut() {
                    pending.hold_applied = true;
                }
            }
        }

        // Nothing on screen: the frontier is pure silence, so hand it straight
        // to the timeline. This is also the path for silence *before* the first
        // word, which is what a muxer needs to start its subtitle pad moving.
        if state.pending.is_none() {
            self.queue_gap(state, out, frontier);
            return;
        }

        let Some(last_word_pts) = state.last_word_pts else {
            return;
        };

        // Clear-timeout is measured from the last word, independent of hold.
        let clear_at = (settings.clear_timeout_ms > 0).then(|| {
            last_word_pts + gst::ClockTime::from_mseconds(settings.clear_timeout_ms as u64)
        });

        if clear_at.is_some_and(|clear_at| frontier >= clear_at) {
            let clear_at = clear_at.unwrap();
            self.close_pending_at(state, out, clear_at, false);
            state.window.clear();
            state.pending = None;
            state.last_word_pts = None;
            // Announce the clear on the cue timeline, not just by going quiet.
            //
            // A GAP moves the frontier but says nothing about the display, so a
            // transport whose cues carry no end — FLV script data, where a cue
            // shows until replaced — cannot tell that the caption should come
            // down. The consumer then has to invent an end from a timeout of
            // its own, and the moment its timeout and `clear-timeout` differ,
            // the caption disappears at the consumer's chosen time instead of
            // this element's.
            //
            // An empty cue at `clear_at` is the same statement CEA-708 makes by
            // erasing the display, expressed in the only vocabulary this
            // transport has.
            if settings.emit_clear_cue {
                self.queue_clear(state, out, clear_at);
            }
            self.queue_gap(state, out, frontier);
            return;
        }

        // Persist: re-emit the identical window in persist-sized cues so the
        // display stays continuous through a pause. Persist starts after the
        // hold-closed cue (pending.start already advanced by rule (b)).
        let persist = gst::ClockTime::from_mseconds(settings.persist_ms.max(1) as u64);
        while let Some(start) = state.pending.as_ref().map(|pending| pending.start) {
            let next_end = start + persist;
            if frontier < next_end || clear_at.is_some_and(|clear_at| next_end >= clear_at) {
                break;
            }
            self.close_pending_at(state, out, next_end, true);
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pts = buffer.pts().ok_or_else(|| {
            gst::error!(CAT, imp = self, "need timestamped buffers");
            gst::FlowError::Error
        })?;

        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "can't map buffer readable");
            gst::FlowError::Error
        })?;

        let data = std::str::from_utf8(map.as_slice()).map_err(|err| {
            gst::error!(CAT, imp = self, "invalid utf8: {err}");
            gst::FlowError::Error
        })?;

        if data.trim().is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut out = Vec::new();
        {
            let mut state = self.state.lock().unwrap();

            // Degraded input: multi-word buffers share the buffer PTS.
            let words: Vec<&str> = data.split_whitespace().collect();
            if words.len() > 1 {
                gst::debug!(
                    CAT,
                    imp = self,
                    "splitting multi-word buffer into {} words at the same pts",
                    words.len()
                );
            }

            for word in words {
                self.handle_word(&mut state, &mut out, pts, word);
            }
        }

        self.flush(out)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::StreamStart(_) => {
                let settings = self.settings.lock().unwrap().clone();
                let mut state = self.state.lock().unwrap();
                state.reset_all(&settings);
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Segment(_) => {
                let mut state = self.state.lock().unwrap();
                state.reset_timeline();
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(_) => {
                let settings = self.settings.lock().unwrap().clone();
                let mut state = self.state.lock().unwrap();
                state.reset_all(&settings);
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Eos(_) => {
                let settings = self.settings.lock().unwrap().clone();
                let mut out = Vec::new();
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(pending) = state.pending.clone() {
                        let hold = gst::ClockTime::from_mseconds(settings.hold_ms as u64);
                        let frontier = state.frontier.unwrap_or(pending.start);
                        let end = frontier.max(pending.start + hold);
                        self.close_pending_at(&mut state, &mut out, end, false);
                    }
                }
                let _ = self.flush(out);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Gap(gap) => {
                let (pts, duration) = gap.get();
                let frontier = pts + duration.unwrap_or(gst::ClockTime::ZERO);

                let settings = self.settings.lock().unwrap().clone();
                let mut out = Vec::new();
                {
                    let mut state = self.state.lock().unwrap();
                    state.seen_gap = true;
                    self.anchor_timeline(&mut state, pts);
                    self.advance_frontier(&mut state, &settings, &mut out, frontier);
                }

                // A flow error here is downstream's business and surfaces on the
                // next chain call; the event itself was handled either way.
                if let Err(err) = self.flush(out) {
                    gst::debug!(CAT, imp = self, "downstream returned {err:?} on gap");
                }
                // Consumed: we own gap emission downstream now.
                true
            }
            EventView::CustomDownstream(custom) => {
                // Forward rstranscribe/final-transcript unchanged.
                if custom
                    .structure()
                    .is_some_and(|s| s.name() == "rstranscribe/final-transcript")
                {
                    return self.srcpad.push_event(event);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let mut peer_query = gst::query::Latency::new();
                if !self.sinkpad.peer_query(&mut peer_query) {
                    return false;
                }
                let (live, min, max) = peer_query.result();
                let hold =
                    gst::ClockTime::from_mseconds(self.settings.lock().unwrap().hold_ms as u64);
                gst::debug!(
                    CAT,
                    imp = self,
                    "reporting latency {hold} on top of upstream {min}"
                );
                q.set(live, min + hold, max.opt_add(hold));
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TextRollup {
    const NAME: &'static str = "GstTextRollup";
    type Type = super::TextRollup;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                TextRollup::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TextRollup::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .query_function(|pad, parent, query| {
                TextRollup::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for TextRollup {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("columns")
                    .nick("Columns")
                    .blurb("Maximum display width per line")
                    .minimum(1)
                    .default_value(DEFAULT_COLUMNS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("lines")
                    .nick("Lines")
                    .blurb("Number of lines in the roll-up window")
                    .minimum(1)
                    .default_value(DEFAULT_LINES)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("hold")
                    .nick("Hold")
                    .blurb(
                        "Maximum wait for a successor word before closing a cue, in \
                         milliseconds. This is the element's latency contribution",
                    )
                    // Zero would leave the hold deadline equal to the cue start,
                    // so rule (b) could never fire and `persist` would silently
                    // own the trailing word instead.
                    .minimum(1)
                    .default_value(DEFAULT_HOLD_MS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("persist")
                    .nick("Persist")
                    .blurb(
                        "During silence, how long each re-emitted identical cue lasts, \
                         in milliseconds",
                    )
                    .minimum(1)
                    .default_value(DEFAULT_PERSIST_MS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("clear-timeout")
                    .nick("Clear Timeout")
                    .blurb(
                        "Silence after which the window clears, in milliseconds. \
                         0 = never clear",
                    )
                    .default_value(DEFAULT_CLEAR_TIMEOUT_MS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("break-on-sentence")
                    .nick("Break On Sentence")
                    .blurb(
                        "Finish the current line at sentence-final punctuation even if \
                         it is not full",
                    )
                    .default_value(DEFAULT_BREAK_ON_SENTENCE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("emit-clear-cue")
                    .nick("Emit Clear Cue")
                    .blurb(
                        "On clear-timeout, also emit an empty cue announcing that the \
                         display is blank. Needed by transports whose cues carry no end \
                         and therefore show until replaced, such as FLV script data",
                    )
                    .default_value(DEFAULT_EMIT_CLEAR_CUE)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        let mut latency_changed = false;
        let mut geometry_changed = false;

        match pspec.name() {
            "columns" => {
                settings.columns = value.get().unwrap();
                geometry_changed = true;
            }
            "lines" => {
                settings.lines = value.get().unwrap();
                geometry_changed = true;
            }
            "hold" => {
                settings.hold_ms = value.get().unwrap();
                latency_changed = true;
            }
            "persist" => settings.persist_ms = value.get().unwrap(),
            "clear-timeout" => settings.clear_timeout_ms = value.get().unwrap(),
            "emit-clear-cue" => settings.emit_clear_cue = value.get().unwrap(),
            "break-on-sentence" => {
                settings.break_on_sentence = value.get().unwrap();
                geometry_changed = true;
            }
            _ => unimplemented!(),
        }

        let snapshot = settings.clone();
        // Dropped before posting: a bus handler is free to call back into this
        // element, and it would deadlock on a lock still held here.
        drop(settings);

        if geometry_changed {
            let mut state = self.state.lock().unwrap();
            state.apply_settings(&snapshot);
        }

        if latency_changed {
            self.post_latency_changed();
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "columns" => settings.columns.to_value(),
            "lines" => settings.lines.to_value(),
            "hold" => settings.hold_ms.to_value(),
            "persist" => settings.persist_ms.to_value(),
            "clear-timeout" => settings.clear_timeout_ms.to_value(),
            "emit-clear-cue" => settings.emit_clear_cue.to_value(),
            "break-on-sentence" => settings.break_on_sentence.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for TextRollup {}

impl ElementImpl for TextRollup {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Text Rollup",
                "Text/Filter",
                "Turn word-level text into a fixed roll-up caption window",
                "Elliott Darfink <elliott.darfink@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let sink = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let src = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src, sink]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::PausedToReady {
            let settings = self.settings.lock().unwrap().clone();
            let mut state = self.state.lock().unwrap();
            state.reset_all(&settings);
        }

        self.parent_change_state(transition)
    }
}
