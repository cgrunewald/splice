//! Putting time into a finished cut, or taking it out, on every track at
//! once: what follows moves along together, so everything stays in sync.

use crate::model::{Clip, ClipId, Generator, Project};
use crate::time::Time;

/// What `insert_time` or `remove_time` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rippled {
    /// Clips that moved along.
    pub moved: Vec<ClipId>,
    /// Clips cut in two at the moment: `(left, right)`; the right half moved.
    pub split: Vec<(ClipId, ClipId)>,
    /// Clips across the moment that got longer (or shorter) instead of being
    /// cut: titles, captions, colours, pictures, and clips on held tracks.
    pub held: Vec<ClipId>,
    /// Clips taken out whole (`remove_time`).
    pub removed: Vec<ClipId>,
    /// Clips on a held track that were cut anyway: their media ran out.
    pub ran_out: Vec<ClipId>,
}

impl Project {
    /// Puts `by` of time in at `at`, on every track: every clip that starts
    /// at or after `at` moves along by `by`. A clip across `at` is cut there
    /// and its right half moves along, unless it holds: titles, captions,
    /// colours, pictures and held frames get longer instead, as does a clip
    /// on a track in `hold` (music, a backdrop) that has the source to. The
    /// keyframes and caption words of a clip that holds keep their moments
    /// in what is shown. The story's beats and cues move with the footage.
    pub fn insert_time(&mut self, at: Time, by: Time, hold: &[usize]) -> Rippled {
        let mut done = Rippled::default();
        if by <= 0 {
            return done;
        }
        let across: Vec<(usize, ClipId)> = self
            .clips()
            .filter(|(_, c)| c.start < at && c.end() > at)
            .map(|(t, c)| (t, c.id))
            .collect();
        let mut holding = Vec::new();
        for (track, id) in across {
            let clip = self.clip(id).expect("listed above");
            if self.holds(clip, hold.contains(&track), by) {
                holding.push(id);
            } else if let Some(right) = self.split(id, at) {
                if hold.contains(&track) {
                    done.ran_out.push(id);
                }
                done.split.push((id, right));
            } else {
                // Too near an end to cut (within a frame): it holds.
                holding.push(id);
            }
        }
        for track in &mut self.tracks {
            for clip in track.clips.iter_mut().filter(|c| c.start >= at) {
                clip.start += by;
                done.moved.push(clip.id);
            }
        }
        for id in holding {
            let Some(clip) = self.clip_mut(id) else { continue };
            let rel = at - clip.start;
            clip.duration += by;
            clip.look.keys.open_gap(rel, by);
            if let Some(Generator::Text(title)) = &mut clip.generator {
                for t in title.timing.iter_mut().filter(|t| **t >= rel) {
                    *t += by;
                }
            }
            done.held.push(id);
        }
        self.story.insert_time(at, by);
        done.moved.sort_by_key(|c| c.0);
        done
    }

    /// Takes the time from `from` to `to` out, on every track: what is in it
    /// goes, and everything after moves up to `from`. A clip across the whole
    /// stretch is cut around it, unless it holds (as in `insert_time`) and
    /// gets shorter instead.
    pub fn remove_time(&mut self, from: Time, to: Time, hold: &[usize]) -> Rippled {
        let mut done = Rippled::default();
        let by = to - from;
        if by <= 0 {
            return done;
        }
        let frame = self.frame();
        let hits: Vec<(usize, ClipId)> = self
            .clips()
            .filter(|(_, c)| c.start < to && c.end() > from)
            .map(|(t, c)| (t, c.id))
            .collect();
        let mut gone = Vec::new();
        for (track, id) in hits {
            let clip = self.clip(id).expect("listed above").clone();
            let (start, end) = (clip.start, clip.end());
            if start >= from && end <= to {
                gone.push(id);
            } else if start < from && end > to {
                // Across the whole stretch: shorter, or cut around it.
                if self.holds(&clip, hold.contains(&track), 0) && clip.duration - by >= frame {
                    let clip = self.clip_mut(id).expect("still here");
                    let rel = from - start;
                    clip.duration -= by;
                    clip.look.keys.close_gap(rel, by);
                    if let Some(Generator::Text(title)) = &mut clip.generator {
                        for t in title.timing.iter_mut() {
                            *t = if *t >= rel + by { *t - by } else { (*t).min(rel) };
                        }
                    }
                    done.held.push(id);
                } else if let Some(middle) = self.split(id, from) {
                    if let Some(right) = self.split(middle, to) {
                        done.split.push((id, right));
                    }
                    gone.push(middle);
                }
            } else if start < from {
                // Runs into the stretch: ends where it starts.
                self.trim_end(id, from);
                if self.clip(id).is_some_and(|c| c.end() > from) {
                    // Too short to keep a frame before it.
                    gone.push(id);
                }
            } else {
                // Starts inside it: begins where it ends, its words with it.
                self.trim_start(id, to);
                let Some(clip) = self.clip_mut(id) else { continue };
                if clip.start < to {
                    gone.push(id);
                } else if let Some(Generator::Text(title)) = &mut clip.generator {
                    let cut = clip.start - start;
                    for t in &mut title.timing {
                        *t = (*t - cut).max(0);
                    }
                }
            }
        }
        for track in &mut self.tracks {
            track.clips.retain(|c| !gone.contains(&c.id));
        }
        done.removed = gone;
        for track in &mut self.tracks {
            for clip in track.clips.iter_mut().filter(|c| c.start >= to) {
                clip.start -= by;
                done.moved.push(clip.id);
            }
            track.clips.sort_by_key(|c| c.start);
        }
        self.story.remove_time(from, to, frame);
        self.prune_tracks();
        done.moved.sort_by_key(|c| c.0);
        done
    }

    /// Whether a clip across an edit point gets longer (or shorter) rather
    /// than being cut: anything with no time of its own, and on a held track
    /// a clip with `more` of its source left to play.
    fn holds(&self, clip: &Clip, held_track: bool, more: Time) -> bool {
        let media = self.media(clip.media);
        let timed = media.is_some_and(|m| m.is_timed()) && !clip.frozen && clip.generator.is_none();
        if !timed {
            return true;
        }
        if !held_track {
            return false;
        }
        let Some(media) = media else { return false };
        let left = if clip.reverse {
            clip.source_in
        } else {
            media.duration - clip.source_out()
        };
        (left as f64 / clip.speed) as Time >= more
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::look::{Keyframe, Prop};
    use crate::model::{Media, MediaId, MediaKind, Title};
    use crate::time::SECOND;
    use std::path::PathBuf;

    fn media(p: &mut Project, kind: MediaKind, seconds: i64, hash: &str) -> MediaId {
        p.add_media(Media {
            id: MediaId(0),
            name: hash.into(),
            hash: hash.into(),
            ext: "mp4".into(),
            kind,
            duration: seconds * SECOND,
            width: 1280,
            height: 720,
            has_audio: kind != MediaKind::Image,
            source: PathBuf::new(),
            recipe: None,
            content: None,
            clicks: Vec::new(),
            sync: 0,
        })
    }

    /// Screen on track 1 (two 4s clips), voice on track 2 (one 8s clip),
    /// music on track 3 (8s of 30s), a caption over the cut on track 4.
    fn cut() -> (Project, [ClipId; 5]) {
        let mut p = Project::default();
        let screen = media(&mut p, MediaKind::Video, 4, "s");
        let voice = media(&mut p, MediaKind::Audio, 8, "v");
        let music = media(&mut p, MediaKind::Audio, 30, "m");
        let a = p.insert_clip(screen, 0, 0).unwrap();
        let b = p.insert_clip(screen, 0, 4 * SECOND).unwrap();
        let v = p.insert_clip(voice, 1, 0).unwrap();
        let m = p.insert_clip(music, 2, 0).unwrap();
        p.trim_end(m, 8 * SECOND);
        let title = Title {
            text: "one two three".into(),
            timing: vec![0, SECOND, 2 * SECOND],
            ..Title::default()
        };
        let c = p.insert_generated(Generator::Text(title), 3, 2 * SECOND, 4 * SECOND);
        (p, [a, b, v, m, c])
    }

    fn span(p: &Project, id: ClipId) -> (Time, Time) {
        let c = p.clip(id).unwrap();
        (c.start, c.end())
    }

    #[test]
    fn inserting_time_moves_every_track_together() {
        let (mut p, [a, b, v, m, c]) = cut();
        p.clip_mut(m).unwrap().look.keys.of_mut(Prop::Opacity).extend([
            Keyframe {
                at: SECOND,
                value: 1.0,
                ease: Default::default(),
            },
            Keyframe {
                at: 5 * SECOND,
                value: 0.0,
                ease: Default::default(),
            },
        ]);
        let one = p.add_beat(None, "one", 3 * SECOND);
        p.add_beat(Some(one), "two", 5 * SECOND);
        p.add_cue(6 * SECOND, "later");

        let done = p.insert_time(3 * SECOND, 2 * SECOND, &[2]);
        // The screen clip across 3s is cut; its right half and the next clip move.
        let (left, right) = done.split.iter().find(|(l, _)| *l == a).copied().unwrap();
        assert_eq!(span(&p, left), (0, 3 * SECOND));
        assert_eq!(span(&p, right), (5 * SECOND, 6 * SECOND));
        assert_eq!(span(&p, b), (6 * SECOND, 10 * SECOND));
        // The voice is cut too: a gap of 2s in it.
        let (_, voice_right) = done.split.iter().find(|(l, _)| *l == v).copied().unwrap();
        assert_eq!(span(&p, voice_right), (5 * SECOND, 10 * SECOND));
        assert_eq!(p.clip(voice_right).unwrap().source_in, 3 * SECOND);
        // Music is on a held track: it plays on, its fade where it was in the picture.
        assert_eq!(span(&p, m), (0, 10 * SECOND));
        let fade: Vec<Time> = p
            .clip(m)
            .unwrap()
            .look
            .keys
            .of(Prop::Opacity)
            .iter()
            .map(|k| k.at)
            .collect();
        assert_eq!(fade, vec![SECOND, 7 * SECOND]);
        // The caption holds; the words said from 3s on light up 2s later,
        // with the voice.
        assert_eq!(span(&p, c), (2 * SECOND, 8 * SECOND));
        let Some(Generator::Text(t)) = &p.clip(c).unwrap().generator else {
            panic!()
        };
        assert_eq!(t.timing, vec![0, 3 * SECOND, 4 * SECOND]);
        assert!(done.held.contains(&m) && done.held.contains(&c));
        // The beat ending there is longer, so the next starts with its footage;
        // the cue after it moved.
        assert_eq!(p.story.beats[0].duration, 5 * SECOND);
        assert_eq!(p.story.beats[1].duration, 5 * SECOND);
        assert_eq!(p.story.cues[0].at, 8 * SECOND);
    }

    #[test]
    fn music_without_enough_source_is_cut_like_the_rest() {
        let (mut p, [_, _, _, m, _]) = cut();
        p.trim_end(m, 8 * SECOND);
        let source_left = 30 * SECOND - 8 * SECOND;
        let done = p.insert_time(3 * SECOND, source_left + SECOND, &[2]);
        assert!(done.split.iter().any(|(l, _)| *l == m));
        assert_eq!(done.ran_out, vec![m]);
    }

    #[test]
    fn removing_time_closes_it_on_every_track() {
        let (mut p, [a, b, v, m, c]) = cut();
        p.add_beat(None, "one", 8 * SECOND);
        let done = p.remove_time(3 * SECOND, 5 * SECOND, &[2]);
        // The screen: the first clip ends at 3s, the second starts there, 1s gone from its head.
        assert_eq!(span(&p, a), (0, 3 * SECOND));
        assert_eq!(span(&p, b), (3 * SECOND, 6 * SECOND));
        assert_eq!(p.clip(b).unwrap().source_in, SECOND);
        // The voice loses the stretch from 3s to 5s of its source.
        let (_, right) = done.split.iter().find(|(l, _)| *l == v).copied().unwrap();
        assert_eq!(span(&p, v), (0, 3 * SECOND));
        assert_eq!(span(&p, right), (3 * SECOND, 6 * SECOND));
        assert_eq!(p.clip(right).unwrap().source_in, 5 * SECOND);
        // Held music is just shorter; the caption across it too.
        assert_eq!(span(&p, m), (0, 6 * SECOND));
        assert_eq!(span(&p, c), (2 * SECOND, 4 * SECOND));
        let Some(Generator::Text(t)) = &p.clip(c).unwrap().generator else {
            panic!()
        };
        assert_eq!(t.timing, vec![0, SECOND, SECOND]);
        assert_eq!(p.story.beats[0].duration, 6 * SECOND);
        assert_eq!(p.duration(), 6 * SECOND);

        // A caption starting inside the stretch keeps its words on time.
        let (mut p, [.., c]) = cut();
        p.remove_time(SECOND, 3 * SECOND, &[]);
        assert_eq!(span(&p, c), (SECOND, 4 * SECOND));
        let Some(Generator::Text(t)) = &p.clip(c).unwrap().generator else {
            panic!()
        };
        assert_eq!(t.timing, vec![0, 0, SECOND]);

        // A clip wholly inside goes.
        let (mut p, [a, b, ..]) = cut();
        let done = p.remove_time(3 * SECOND, 9 * SECOND, &[]);
        assert!(done.removed.contains(&b));
        assert_eq!(span(&p, a), (0, 3 * SECOND));
    }
}
