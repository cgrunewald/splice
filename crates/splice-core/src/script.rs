//! The script: the words said in each beat. A script written elsewhere comes
//! in as beats (a heading starts one, a line in square brackets is a
//! direction for the cut), the teleprompter shows it while a take is
//! recorded, and what was heard in the take is matched back to each beat's
//! lines so the reading can be cut to the story.

use crate::captions::{Word, unspaced};
use crate::model::{Clip, ClipId, Project};
use crate::story::BeatId;
use crate::time::{SECOND, Time};
use serde::{Deserialize, Serialize};

/// Words a minute, read aloud at an easy pace.
pub const PACE: u32 = 150;

/// How the teleprompter shows the script over the Record tab's scene.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompter {
    pub on: bool,
    /// Words a minute.
    pub pace: u32,
    /// Letter size, in points.
    pub size: f32,
}

impl Default for Prompter {
    fn default() -> Prompter {
        Prompter {
            on: false,
            pace: PACE,
            size: 40.0,
        }
    }
}

impl Prompter {
    pub const PACES: (u32, u32) = (60, 320);
    pub const SIZES: (f32, f32) = (20.0, 96.0);

    pub fn is_default(&self) -> bool {
        *self == Prompter::default()
    }

    pub fn clamped(self) -> Prompter {
        Prompter {
            pace: self.pace.clamp(Self::PACES.0, Self::PACES.1),
            size: self.size.clamp(Self::SIZES.0, Self::SIZES.1),
            ..self
        }
    }
}

/// A stretch of a script: one beat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Section {
    pub title: String,
    /// What is said.
    pub lines: String,
    /// Notes for the cut, from `[square brackets]`.
    pub directions: Vec<String>,
}

/// Reads a script as sections. A line starting with `#` begins a section and
/// names it; without any, each paragraph is one, named by its first words.
/// Anything in `[square brackets]` is a direction, not said.
pub fn parse(text: &str) -> Vec<Section> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let heading = |line: &str| line.trim_start().starts_with('#');
    let headed = text.lines().any(heading);
    let mut sections: Vec<Section> = Vec::new();
    let mut open = false;
    let mut gap = false;
    for line in text.lines() {
        let line = line.trim();
        if headed && heading(line) {
            sections.push(Section {
                title: line.trim_start_matches('#').trim().to_string(),
                ..Section::default()
            });
            (open, gap) = (true, false);
            continue;
        }
        if line.is_empty() {
            // A paragraph ends: without headings, so does the section.
            if !headed {
                open = false;
            }
            gap = true;
            continue;
        }
        if !open {
            sections.push(Section::default());
            open = true;
            gap = false;
        }
        let section = sections.last_mut().expect("opened above");
        let (said, directions) = split_directions(line);
        section.directions.extend(directions);
        if !said.is_empty() {
            if !section.lines.is_empty() {
                section.lines.push_str(if gap { "\n\n" } else { "\n" });
            }
            section.lines.push_str(&said);
            gap = false;
        }
    }
    for s in &mut sections {
        if s.title.is_empty() {
            s.title = title_of(&s.lines);
        }
    }
    sections
        .into_iter()
        .filter(|s| !(s.title.is_empty() && s.lines.is_empty() && s.directions.is_empty()))
        .collect()
}

/// A line's spoken words, and the directions bracketed in it.
fn split_directions(line: &str) -> (String, Vec<String>) {
    let mut said = String::new();
    let mut directions = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']') else { break };
        said.push_str(&rest[..open]);
        let note = rest[open + 1..open + close].trim();
        if !note.is_empty() {
            directions.push(note.to_string());
        }
        rest = &rest[open + close + 1..];
    }
    said.push_str(rest);
    (said.split_whitespace().collect::<Vec<_>>().join(" "), directions)
}

/// A name for lines without one: their first few words.
fn title_of(lines: &str) -> String {
    const WORDS: usize = 4;
    let mut words = lines.split_whitespace();
    let first: Vec<&str> = words.by_ref().take(WORDS).collect();
    let title = first.join(" ");
    let title = title.trim_end_matches(|c: char| !c.is_alphanumeric()).to_string();
    if words.next().is_some() {
        format!("{title}…")
    } else {
        title
    }
}

/// How many words are said in `text`. Writing without spaces (Japanese,
/// Chinese) counts two characters a word.
pub fn words(text: &str) -> usize {
    text.split_whitespace()
        .map(|w| {
            let dense = w.chars().filter(|c| unspaced(*c) && c.is_alphanumeric()).count();
            let plain = w.chars().any(|c| c.is_alphanumeric() && !unspaced(c));
            dense.div_ceil(2) + usize::from(plain)
        })
        .sum()
}

/// How long `text` takes to say at `pace` words a minute, to the half second.
pub fn read_time(text: &str, pace: u32) -> Time {
    let n = words(text) as Time;
    let t = n * 60 * SECOND / pace.max(1) as Time;
    let half = SECOND / 2;
    (t + half - 1) / half * half
}

/// Where the reader is, `read` words into sections of `counts` words each:
/// the section, and how far through it (0 to 1). Sections without words are
/// passed over. Past the end: the last section, fully read.
pub fn place(counts: &[usize], read: f32) -> (usize, f32) {
    let mut before = 0.0;
    let mut last = None;
    for (i, n) in counts.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let n = *n as f32;
        if read < before + n {
            return (i, ((read - before) / n).clamp(0.0, 1.0));
        }
        before += n;
        last = Some(i);
    }
    (last.unwrap_or(0), if last.is_some() { 1.0 } else { 0.0 })
}

/// Where a beat's lines were read in a recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// From just before its first word to just after its last.
    pub start: Time,
    pub end: Time,
    /// How much of the script was heard there: 1 is every word.
    pub score: f32,
    /// What was heard there.
    pub heard: String,
}

/// A reading must have at least this much of its script to count.
const HEARD_ENOUGH: f32 = 0.6;

/// Air left around a reading, so words are not clipped.
const AIR: Time = SECOND * 3 / 20;

/// The pieces a word is compared by: lower case, no punctuation; each
/// character of unspaced writing on its own.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if unspaced(c) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            if c.is_alphanumeric() {
                out.push(c.to_string());
            }
        } else if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        } else if (c.is_whitespace() || matches!(c, '-' | '–' | '—' | '/')) && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Finds where each script was read in what was `heard`, in order. A script
/// read more than once (a fluffed line said again) is found where it was
/// last read well; one never read, or mostly missed, is None.
pub fn align(scripts: &[&str], heard: &[Word]) -> Vec<Option<Reading>> {
    // Each heard token, and the word it came from.
    let said: Vec<(String, usize)> = heard
        .iter()
        .enumerate()
        .flat_map(|(i, w)| tokens(&w.text).into_iter().map(move |t| (t, i)))
        .collect();
    let mut from = 0;
    scripts
        .iter()
        .map(|script| {
            let want = tokens(script);
            let (first, end, missed) = find(&want, &said[from..])?;
            let score = 1.0 - missed as f32 / want.len() as f32;
            if score < HEARD_ENOUGH || first >= end {
                return None;
            }
            let (a, z) = (said[from + first].1, said[from + end - 1].1);
            from += end;
            // A little air, but never into the words either side.
            let start = match a.checked_sub(1).map(|i| heard[i].end) {
                Some(before) => (heard[a].start - AIR).max((before + heard[a].start) / 2),
                None => (heard[a].start - AIR).max(0),
            };
            let end = match heard.get(z + 1).map(|w| w.start) {
                Some(after) => (heard[z].end + AIR).min((heard[z].end + after) / 2),
                None => heard[z].end + AIR,
            };
            Some(Reading {
                start: start.min(heard[a].start),
                end: end.max(heard[z].end),
                score,
                heard: heard[a..=z]
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            })
        })
        .collect()
}

/// The stretch of `said` that reads `want` best, as (first token, one past
/// the last, words missed or misheard): the last such stretch when it was
/// read more than once.
fn find(want: &[String], said: &[(String, usize)]) -> Option<(usize, usize, usize)> {
    let (n, m) = (want.len(), said.len());
    if n == 0 || m == 0 {
        return None;
    }
    // (cost, where the stretch starts), for `want[..i]` read ending at each token.
    let mut row: Vec<(usize, usize)> = (0..=m).map(|j| (0, j)).collect();
    for i in 1..=n {
        let mut next = Vec::with_capacity(m + 1);
        next.push((i, 0));
        for j in 1..=m {
            let same = want[i - 1] == said[j - 1].0;
            let heard = (row[j - 1].0 + usize::from(!same), row[j - 1].1);
            let missed = (row[j].0 + 1, row[j].1);
            let extra = (next[j - 1].0 + 1, next[j - 1].1);
            let mut best = heard;
            for other in [missed, extra] {
                if other.0 < best.0 {
                    best = other;
                }
            }
            next.push(best);
        }
        row = next;
    }
    let best = row.iter().map(|(cost, _)| *cost).min()?;
    // Said again later about as well: the later one is the keeper.
    let near = best + n / 10;
    let mut end = (0..=m).rev().find(|j| row[*j].0 <= near)?;
    while end > 0 && row[end - 1].0 < row[end].0 {
        end -= 1;
    }
    Some((row[end].1, end, row[end].0))
}

impl Project {
    /// Adds a script to the story: a beat for each section after the beats
    /// there, as long as its lines take to say at `pace` words a minute.
    pub fn add_script(&mut self, text: &str, pace: u32) -> Vec<BeatId> {
        parse(text)
            .into_iter()
            .map(|section| {
                let said = read_time(&section.lines, pace);
                let length = if said == 0 {
                    crate::story::BEAT_DURATION
                } else {
                    said.max(2 * SECOND)
                };
                let id = self.add_beat(None, &section.title, length);
                self.set_beat(id, |b| b.script = section.lines);
                for note in &section.directions {
                    self.add_direction(id, note);
                }
                id
            })
            .collect()
    }

    /// Whether any beat has lines to say.
    pub fn has_script(&self) -> bool {
        self.story.beats.iter().any(|b| !b.script.trim().is_empty())
    }

    /// The story as a script: each beat's title as a heading, its lines, and
    /// its directions in square brackets. Reads back with `parse`.
    pub fn script_text(&self) -> String {
        let mut out = String::new();
        for beat in &self.story.beats {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("# {}\n", beat.title.trim()));
            if !beat.script.trim().is_empty() {
                out.push_str(&format!("\n{}\n", beat.script.trim()));
            }
            let notes: Vec<&str> = beat
                .directions
                .iter()
                .map(|d| d.note.trim())
                .filter(|n| !n.is_empty())
                .collect();
            if !notes.is_empty() {
                out.push('\n');
                for note in notes {
                    out.push_str(&format!("[{note}]\n"));
                }
            }
        }
        out
    }

    /// Cuts a recording of the script being read to the story. `layers` are
    /// the take's clips (its screen, camera, microphone: cut alike), and
    /// `spans` say where on the timeline each beat's lines were read. Each
    /// beat becomes as long as its reading and gets that piece of every
    /// layer at its start; the rest of the take (false starts, lines said
    /// again, the gaps) leaves the timeline. Returns the pieces.
    pub fn lay_reading(&mut self, layers: &[ClipId], spans: &[(BeatId, Time, Time)]) -> Vec<ClipId> {
        let frame = self.frame();
        let spans: Vec<(BeatId, Time, Time)> = spans
            .iter()
            .filter(|(b, a, z)| self.story.index(*b).is_some() && *z - *a >= frame)
            .copied()
            .collect();
        // The take as it lies now: beats changing length will move it.
        let takes: Vec<Clip> = layers
            .iter()
            .filter_map(|id| self.clip(*id))
            .filter(|c| c.generator.is_none() && !c.is_retimed())
            .cloned()
            .collect();
        if spans.is_empty() || takes.is_empty() {
            return Vec::new();
        }
        for (beat, a, z) in &spans {
            self.set_beat_duration(*beat, z - a);
        }
        let mut pieces = Vec::new();
        for take in &takes {
            let Some(track) = self.slot(take.id).map(|s| s.track) else {
                continue;
            };
            self.tracks[track].clips.retain(|c| c.id != take.id);
            for (beat, a, z) in &spans {
                let (from, to) = ((*a).max(take.start), (*z).min(take.end()));
                let Some((start, _)) = self.story.span(*beat).filter(|_| to - from >= frame) else {
                    continue;
                };
                let id = ClipId(self.alloc());
                let piece = Clip {
                    id,
                    start: start + (from - a),
                    source_in: take.source_in + (from - take.start),
                    duration: to - from,
                    transition: None,
                    ..take.clone()
                };
                self.place(piece, track);
                pieces.push(id);
            }
        }
        self.prune_tracks();
        pieces
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Media, MediaId, MediaKind};

    #[test]
    fn headings_start_beats_and_brackets_are_directions() {
        let text = "Before we start.\n\n# Hook\nYou ship on Fridays. [zoom in on the calendar]\nSo do we.\n\n\
                    Second paragraph.\n[Fade out]\n\n## The fix \n\n# Empty\n";
        let s = parse(text);
        assert_eq!(s.len(), 4, "{s:?}");
        assert_eq!(s[0].title, "Before we start");
        assert_eq!(s[1].title, "Hook");
        assert_eq!(s[1].lines, "You ship on Fridays.\nSo do we.\n\nSecond paragraph.");
        assert_eq!(s[1].directions, vec!["zoom in on the calendar", "Fade out"]);
        assert_eq!(s[2].title, "The fix");
        assert_eq!((s[3].title.as_str(), s[3].lines.as_str()), ("Empty", ""));
    }

    #[test]
    fn without_headings_each_paragraph_is_a_beat() {
        let s = parse("One two three four five six.\nStill the first.\r\n\r\nSecond, short.\n\n\n[Cut to the app]\n");
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[0].title, "One two three four…");
        assert_eq!(s[0].lines, "One two three four five six.\nStill the first.");
        assert_eq!(s[1].title, "Second, short");
        assert_eq!(s[2].directions, vec!["Cut to the app"]);
        assert!(parse("  \n\n").is_empty());
    }

    #[test]
    fn words_are_counted_and_timed() {
        assert_eq!(words("Hello, world — it's 3 o'clock."), 5);
        assert_eq!(words("今日は動画"), 3);
        assert_eq!(read_time("", PACE), 0);
        let text = vec!["word"; 150].join(" ");
        assert_eq!(read_time(&text, 150), 60 * SECOND);
        assert_eq!(
            read_time("one two three", 150),
            3 * SECOND / 2,
            "to the half second above"
        );
    }

    #[test]
    fn the_reader_is_placed_in_a_section() {
        let counts = [4, 0, 6];
        assert_eq!(place(&counts, 0.0), (0, 0.0));
        assert_eq!(place(&counts, 2.0), (0, 0.5));
        assert_eq!(place(&counts, 7.0), (2, 0.5));
        assert_eq!(place(&counts, 99.0), (2, 1.0));
        assert_eq!(place(&[], 3.0), (0, 0.0));
    }

    fn heard(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                text: w.to_string(),
                start: i as Time * SECOND,
                end: i as Time * SECOND + SECOND / 2,
            })
            .collect()
    }

    #[test]
    fn readings_are_found_in_order_keeping_the_last_good_one() {
        let said = heard(
            "okay rolling. You ship on Fridays so do — sorry. You ship on Fridays, so do we! \
             um here is the fix: test it first. thanks",
        );
        let found = align(
            &[
                "You ship on Fridays. So do we.",
                "Never said at all, this one.",
                "Here is the fix. Test it first.",
            ],
            &said,
        );
        let hook = found[0].as_ref().expect("read twice");
        // The second reading: words 10 to 16, with a little air either side.
        assert_eq!(hook.heard, "You ship on Fridays, so do we!");
        assert_eq!(hook.score, 1.0);
        assert_eq!(hook.start, 10 * SECOND - AIR);
        assert_eq!(hook.end, 16 * SECOND + SECOND / 2 + AIR);
        assert!(found[1].is_none());
        let fix = found[2].as_ref().expect("read once");
        assert_eq!(fix.heard, "here is the fix: test it first.");
        assert!(fix.start >= hook.end);
    }

    #[test]
    fn a_reading_with_a_slip_still_counts_and_nothing_heard_is_nothing() {
        let said = heard("the quick brown box jumps over a lazy dog");
        let found = align(&["The quick brown fox jumps over the lazy dog."], &said);
        let r = found[0].as_ref().unwrap();
        assert!((r.score - 7.0 / 9.0).abs() < 0.01, "{r:?}");
        assert_eq!(r.start, 0);
        assert!(align(&["Hello there"], &[]).iter().all(Option::is_none));
        assert!(align(&["", "[ ]"], &said).iter().all(Option::is_none));
    }

    #[test]
    fn a_script_comes_in_as_beats_and_reads_back() {
        let mut p = Project::default();
        let text = "# Hook\n\nYou ship on Fridays.\n\n[Zoom in]\n\n# Fix\n\nTest it first.\n";
        let ids = p.add_script(text, PACE);
        assert_eq!(ids.len(), 2);
        let hook = &p.story.beats[0];
        assert_eq!(
            (hook.title.as_str(), hook.script.as_str()),
            ("Hook", "You ship on Fridays.")
        );
        assert_eq!(hook.directions[0].note, "Zoom in");
        assert_eq!(hook.duration, 2 * SECOND, "never shorter than two seconds");
        assert!(p.has_script());
        assert_eq!(p.script_text(), text);
        // A second script follows the first.
        p.add_script("More.", PACE);
        assert_eq!(p.story.beats.len(), 3);
    }

    #[test]
    fn a_reading_is_cut_to_the_story() {
        let mut p = Project::default();
        let mut media = |name: &str, kind| {
            p.add_media(Media {
                id: MediaId(0),
                name: name.into(),
                hash: name.into(),
                ext: "mp4".into(),
                kind,
                duration: 30 * SECOND,
                width: 1280,
                height: 720,
                has_audio: true,
                source: Default::default(),
                recipe: None,
                content: None,
                clicks: Vec::new(),
            })
        };
        let (camera, mic) = (media("camera", MediaKind::Video), media("mic", MediaKind::Audio));
        let ids = p.add_script(
            "# A\n\none two three\n\n# B\n\nnever read\n\n# C\n\nfour five six\n",
            PACE,
        );
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        // The take, laid after the story's six seconds.
        let at = 10 * SECOND;
        let video = p.insert_clip(camera, 0, at).unwrap();
        let sound = p.insert_clip(mic, 1, at).unwrap();
        p.set_look(video, |l| l.scale = 0.5);
        // A read from 2s to 5s of the take, C from 12s to 16.5s.
        let spans = [
            (a, at + 2 * SECOND, at + 5 * SECOND),
            (c, at + 12 * SECOND, at + 16 * SECOND + SECOND / 2),
        ];
        let pieces = p.lay_reading(&[video, sound], &spans);
        assert_eq!(pieces.len(), 4);
        assert!(
            p.clip(video).is_none() && p.clip(sound).is_none(),
            "the rest of the take goes"
        );
        assert_eq!(p.story.span(a), Some((0, 3 * SECOND)));
        let b_length = p.story.span(b).map(|(s, e)| e - s).unwrap();
        let c_start = 3 * SECOND + b_length;
        assert_eq!(p.story.span(c), Some((c_start, c_start + 4 * SECOND + SECOND / 2)));
        for beat in [a, c] {
            let clips = p.beat_clips(beat);
            assert_eq!(clips.len(), 2, "a piece of each layer");
            let (start, end) = p.story.span(beat).unwrap();
            for id in clips {
                let clip = p.clip(id).unwrap();
                assert_eq!((clip.start, clip.end()), (start, end));
                let into = if beat == a { 2 * SECOND } else { 12 * SECOND };
                assert_eq!(clip.source_in, into);
                if clip.media == camera {
                    assert_eq!(clip.look.scale, 0.5, "pieces keep the take's look");
                }
            }
        }
        assert!(p.beat_clips(b).is_empty());
        assert_eq!(p.tracks.len(), 2, "each layer stays on its track");
        // Nothing to lay: nothing changes.
        let before = p.clone();
        assert!(p.lay_reading(&[ClipId(9999)], &spans).is_empty());
        assert_eq!(p, before);
    }
}
