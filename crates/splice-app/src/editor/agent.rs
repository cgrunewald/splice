//! The editor's half of `splice ctl`: answering requests from other programs
//! against the live project, and the conversation with a connected assistant.
//!
//! Edits from outside go through the same `edit` as the mouse and keys, so
//! each is one undo step, autosaves, and shows at once. Nothing here moves
//! the person's selection or playhead unless the request is about those.

use super::*;
use crate::ctl::{Incoming, fail};
use crate::titles;
use serde_json::{Value, json};
use splice_core::time::from_secs;
use std::collections::VecDeque;
use std::sync::mpsc::Sender;

/// An assistant blocked in `splice ctl wait`.
struct Waiter {
    id: u64,
    name: String,
    reply: Sender<String>,
}

#[derive(Default)]
pub struct Agent {
    waiters: VecDeque<Waiter>,
    /// Messages sent while the assistant was busy with an earlier one.
    outbox: VecDeque<String>,
    /// The assistant took a message and has not come back for another yet.
    working: Option<String>,
}

/// How the title bar shows the assistant.
pub enum Presence {
    Listening(String),
    Working(String),
}

impl Agent {
    pub fn presence(&self) -> Option<Presence> {
        if let Some(name) = &self.working {
            return Some(Presence::Working(name.clone()));
        }
        self.waiters.front().map(|w| Presence::Listening(w.name.clone()))
    }
}

fn s(t: Time) -> f64 {
    (secs(t) * 1000.0).round() / 1000.0
}

/// `#rrggbb`, or `#rrggbbaa` when not opaque.
fn hex(color: u32) -> String {
    if color & 0xff == 0xff {
        format!("#{:06x}", color >> 8)
    } else {
        format!("#{color:08x}")
    }
}

/// A colour from `#rgb`, `#rrggbb`, `#rrggbbaa` or a few names: 0xRRGGBBAA.
pub fn parse_color(text: &str) -> Option<u32> {
    let named = match text.to_lowercase().as_str() {
        "white" => Some(0xffffffff),
        "black" => Some(0x000000ff),
        "red" => Some(0xe5484dff),
        "green" => Some(0x00ff00ff),
        "blue" => Some(0x0047bbff),
        "yellow" | "gold" => Some(0xf2c14eff),
        "paper" => Some(0xf3eee4ff),
        "ink" => Some(0x12151fff),
        "vermilion" | "accent" => Some(0xef6a55ff),
        "clear" | "transparent" => Some(0),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let digits = text.trim_start_matches('#');
    let value = u32::from_str_radix(digits, 16).ok()?;
    match digits.len() {
        3 => {
            let (r, g, b) = ((value >> 8) & 0xf, (value >> 4) & 0xf, value & 0xf);
            Some((r * 17) << 24 | (g * 17) << 16 | (b * 17) << 8 | 0xff)
        }
        6 => Some(value << 8 | 0xff),
        8 => Some(value),
        _ => None,
    }
}

pub(super) fn look_json(look: &splice_core::look::Look) -> Value {
    json!({
        "x": look.x,
        "y": look.y,
        "scale": look.scale,
        "opacity": look.opacity,
        "enter": { "kind": look.enter.kind.name(), "duration": s(look.enter.duration) },
        "exit": { "kind": look.exit.kind.name(), "duration": s(look.exit.duration) },
        "drift": look.drift,
        "key": look.key.map(|k| json!({
            "color": hex(k.color << 8 | 0xff),
            "similarity": k.similarity,
            "blend": k.blend,
            "despill": k.despill,
        })),
        "mask": look.mask,
        "crop": look.crop,
        "effects": (!look.fx.is_none()).then_some(look.fx),
        "backdrop": look.backdrop,
        "redact": look.redact,
        "mirror": look.mirror,
        "grade": (!look.grade.is_none()).then(|| json!({
            "look": look.grade.preset.name(),
            "exposure": look.grade.exposure,
            "contrast": look.grade.contrast,
            "saturation": look.grade.saturation,
            "warmth": look.grade.temperature,
            "tint": look.grade.tint,
        })),
        "keyframes": (!look.keys.is_empty()).then(|| keyframes_json(look)),
    })
}

/// Keyframes per property: clip time (seconds from its start), value, ease.
fn keyframes_json(look: &splice_core::look::Look) -> Value {
    let mut out = serde_json::Map::new();
    for prop in splice_core::look::Prop::ALL {
        let keys = look.keys.of(prop);
        if keys.is_empty() {
            continue;
        }
        out.insert(
            prop.name().into(),
            json!(
                keys.iter()
                    .map(|k| json!({ "at": s(k.at), "value": k.value, "ease": k.ease.name() }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    Value::Object(out)
}

fn kind(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Image => "image",
        MediaKind::Audio => "audio",
    }
}

impl Editor {
    pub(super) fn ctl_incoming(&mut self, incoming: Incoming, window: &mut Window, cx: &mut Context<Self>) {
        match incoming {
            Incoming::Gone(id) => {
                self.agent.waiters.retain(|w| w.id != id);
                cx.notify();
            }
            Incoming::Request { id, line, reply } => {
                let request: Value = match serde_json::from_str(&line) {
                    Ok(request) => request,
                    Err(_) => {
                        let _ = reply.send(fail("the request is not JSON").to_string());
                        return;
                    }
                };
                // Requests that answer later keep the reply; the rest answer now.
                if let Some(answer) = self.ctl_request(id, &request, reply.clone(), window, cx) {
                    let _ = reply.send(answer.to_string());
                }
                cx.notify();
            }
        }
    }

    fn ctl_request(
        &mut self,
        id: u64,
        request: &Value,
        reply: Sender<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Value> {
        let text = |key: &str| request.get(key).and_then(Value::as_str);
        let flag = |key: &str| request.get(key).and_then(Value::as_bool);
        let cmd = text("cmd").unwrap_or("");
        let answer = match cmd {
            "score" | "scene" | "open" => return self.ctl_made(cmd, request, reply, window, cx),
            "story" if request["sub"] == json!("shot") => return self.ctl_shot(request, reply, cx),
            "story" if matches!(request["sub"].as_str(), Some("cast" | "brief" | "take")) => {
                return self.ctl_plan(request, reply, cx);
            }
            "story" if matches!(request["sub"].as_str(), Some("match" | "cut")) => {
                return self.ctl_reading(request, reply, cx);
            }
            "bible" => return self.ctl_bible(request, reply, cx),
            "captions" => return self.ctl_captions(request, reply, cx),
            "transition" | "speed" | "freeze" | "keyframe" | "leak" | "zoom" => self.ctl_effects(cmd, request, cx),
            "story" | "cue" | "view" => self.ctl_story(cmd, request, cx),
            "record" => return self.ctl_record(request, reply, cx),
            "symbol" | "placeholder" | "hold" => return self.ctl_symbol(cmd, request, reply, cx),
            "ui" | "click" | "hover" | "drag" | "scroll" | "key" | "type" => {
                return self.ctl_input(cmd, request, reply, window, cx);
            }
            "who" => json!({
                "ok": true,
                "project": self.doc.path(),
                "name": self.doc.name(),
                "pid": std::process::id(),
            }),
            "status" => self.ctl_status(),
            "read" => self.ctl_read(),
            "versions" => {
                let list: Vec<Value> = self
                    .doc
                    .versions()
                    .iter()
                    .map(|v| {
                        json!({
                            "id": v.id,
                            "kind": match v.kind { VersionKind::Save => "saved", VersionKind::Auto => "autosave" },
                            "label": v.label,
                            "ago": render::ago(SystemTime::now(), v.created),
                        })
                    })
                    .collect();
                json!({ "ok": true, "versions": list })
            }
            "restore" => match request.get("version").and_then(Value::as_i64) {
                Some(version) => {
                    self.restore(version, window, cx);
                    self.edited()
                }
                None => fail("name the version with `version` (an id from `splice ctl versions`)"),
            },
            "import" => {
                let files: Vec<PathBuf> = request["files"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect())
                    .unwrap_or_default();
                if files.is_empty() {
                    return Some(fail("`files` is missing"));
                }
                if let Some(missing) = files.iter().find(|f| !f.exists()) {
                    return Some(fail(format!("{} does not exist", missing.display())));
                }
                let place = match self.time(request, "at") {
                    Ok(Some(t)) => Some((t, self.track(request).unwrap_or(0))),
                    Ok(None) => None,
                    Err(e) => return Some(fail(e)),
                };
                self.import_paths(files, place, Some(reply), cx);
                return None;
            }
            "add" => {
                let media = match self.find_media(&request["media"]) {
                    Ok(media) => media,
                    Err(e) => return Some(fail(e)),
                };
                let track = self.track(request).unwrap_or(0);
                let at = match self.time(request, "at") {
                    Ok(Some(t)) => t,
                    Ok(None) => self.doc.project.tracks.get(track).map_or(0, |t| t.end()),
                    Err(e) => return Some(fail(e)),
                };
                let lengths = self.lengths();
                let whole = self
                    .doc
                    .project
                    .media(media)
                    .filter(|m| m.is_timed())
                    .map(|m| m.duration);
                match self.edit(cx, |p| p.insert_clip(media, track, at)) {
                    Some(clip) => {
                        let mut reply = json!({ "clip": self.clip_json(clip) });
                        let was = lengths.into_iter().chain(whole.map(|d| (clip, d))).collect::<Vec<_>>();
                        self.say_how_placed(&mut reply, &was, &[(clip, track)]);
                        self.edited_with(reply)
                    }
                    None => fail("that media has nothing to place"),
                }
            }
            "move" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                let first = self.doc.project.slot(ids[0]).expect("checked");
                let start = self.doc.project.clip(ids[0]).map_or(0, |c| c.start);
                let dt = match self.time(request, "to") {
                    Ok(Some(to)) => to - start,
                    Ok(None) => 0,
                    Err(e) => return Some(fail(e)),
                };
                let dtrack = self.track(request).map_or(0, |t| t as isize - first.track as isize);
                let lengths = self.lengths();
                let asked: Vec<(ClipId, usize)> = ids
                    .iter()
                    .filter_map(|id| Some((*id, self.doc.project.slot(*id)?.track)))
                    .map(|(id, t)| (id, (t as isize + dtrack).max(0) as usize))
                    .collect();
                self.edit(cx, |p| p.move_clips(&ids, dt, dtrack));
                let clips: Vec<Value> = ids.iter().map(|id| self.clip_json(*id)).collect();
                let mut reply = json!({ "clips": clips });
                self.say_how_placed(&mut reply, &lengths, &asked);
                // Moved earlier than the timeline's start: stopped at 0, the
                // others keeping their spacing.
                let got = self.doc.project.clip(ids[0]).map_or(start, |c| c.start);
                if (got - (start + dt)).abs() > self.doc.project.frame() / 2 {
                    reply["clamped"] = json!([{
                        "clip": ids[0].0, "asked": s(start + dt), "got": s(got),
                        "by": "the start of the timeline (clips moved together keep their spacing)",
                    }]);
                    let mut warnings = reply["warnings"].as_array().cloned().unwrap_or_default();
                    warnings.push(json!(
                        "moved that far, a clip would start before 0: the earliest starts at 0 and the rest keep \
                         their spacing (`clamped`)"
                    ));
                    reply["warnings"] = json!(warnings);
                }
                self.edited_with(reply)
            }
            "trim" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                let (start, end) = match (self.time(request, "start"), self.time(request, "end")) {
                    (Ok(start), Ok(end)) => (start, end),
                    (Err(e), _) | (_, Err(e)) => return Some(fail(e)),
                };
                if start.is_none() && end.is_none() {
                    return Some(fail("give `start`, `end` or both: where the clip's edges go"));
                }
                self.edit(cx, |p| {
                    for &id in &ids {
                        if let Some(t) = start {
                            p.trim_start(id, t);
                        }
                        if let Some(t) = end {
                            p.trim_end(id, t);
                        }
                    }
                });
                let clips: Vec<Value> = ids.iter().map(|id| self.clip_json(*id)).collect();
                let mut reply = self.edited_with(json!({ "clips": clips }));
                let clamped: Vec<Value> = ids
                    .iter()
                    .flat_map(|id| {
                        [("start", start), ("end", end)]
                            .into_iter()
                            .filter_map(|(edge, asked)| self.trim_stopped(*id, edge, asked?))
                    })
                    .collect();
                if !clamped.is_empty() {
                    reply["clamped"] = json!(clamped);
                    let note = "an edge stopped short of where it was asked to go: see `clamped`";
                    reply["note"] = json!(note);
                    reply["warnings"] = json!([note]);
                }
                reply
            }
            "split" => {
                let at = match self.time(request, "at") {
                    Ok(t) => t.unwrap_or(self.player.position),
                    Err(e) => return Some(fail(e)),
                };
                let ids = if request.get("clips").is_some() {
                    match self.clips(request) {
                        Ok(ids) => ids,
                        Err(e) => return Some(fail(e)),
                    }
                } else {
                    Vec::new()
                };
                let made = self.edit(cx, |p| p.split_at(at, &ids));
                if made.is_empty() {
                    fail(format!("nothing to split at {:.3}s", secs(at)))
                } else {
                    let clips: Vec<Value> = made.iter().map(|id| self.clip_json(*id)).collect();
                    self.edited_with(json!({ "new": clips }))
                }
            }
            "delete" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                if request.get("ripple").is_some_and(Value::is_string) {
                    return Some(fail(
                        "delete --ripple takes no value: it closes the gap on each clip's track",
                    ));
                }
                if flag("ripple") == Some(true) {
                    self.edit(cx, |p| p.ripple_delete(&ids));
                } else {
                    self.edit(cx, |p| p.delete(&ids));
                }
                self.edited()
            }
            "insert-time" | "remove-time" => {
                let hold: Vec<usize> = match request.get("hold") {
                    None => Vec::new(),
                    Some(v) => {
                        let tracks = self.doc.project.tracks.len();
                        let picked: Option<Vec<usize>> = v.as_array().and_then(|a| {
                            a.iter()
                                .map(|n| {
                                    n.as_u64()
                                        .filter(|n| (1..=tracks as u64).contains(n))
                                        .map(|n| n as usize - 1)
                                })
                                .collect()
                        });
                        match picked {
                            Some(p) => p,
                            None => return Some(fail(format!("--hold names tracks, 1 to {tracks}"))),
                        }
                    }
                };
                let done = if cmd == "insert-time" {
                    let at = match self.time(request, "at") {
                        Ok(Some(t)) => t,
                        Ok(None) => return Some(fail("say where with --at T")),
                        Err(e) => return Some(fail(e)),
                    };
                    let Some(by) = request.get("for").and_then(Value::as_f64).filter(|d| *d > 0.0) else {
                        return Some(fail("say how long with --for SECONDS"));
                    };
                    let by = from_secs(by);
                    self.edit(cx, |p| p.insert_time(at, by, &hold))
                } else {
                    let (from, to) = match (self.time(request, "from"), self.time(request, "to")) {
                        (Ok(Some(a)), Ok(Some(b))) if b > a => (a, b),
                        (Ok(Some(_)), Ok(Some(_))) => return Some(fail("--to is after --from")),
                        (Err(e), _) | (_, Err(e)) => return Some(fail(e)),
                        _ => return Some(fail("say which stretch with --from A --to B")),
                    };
                    self.edit(cx, |p| p.remove_time(from, to, &hold))
                };
                let ids = |ids: &[ClipId]| json!(ids.iter().map(|c| c.0).collect::<Vec<_>>());
                let mut answer = self.edited_with(json!({
                    "moved": ids(&done.moved),
                    "split": done.split.iter().map(|(l, r)| json!({ "clip": l.0, "right": r.0 })).collect::<Vec<_>>(),
                    "held": ids(&done.held),
                }));
                if !done.removed.is_empty() {
                    answer["removed"] = ids(&done.removed);
                }
                if !done.ran_out.is_empty() {
                    answer["ran_out"] = ids(&done.ran_out);
                    answer["note"] =
                        json!("clips on a held track had no more source to play, so they were cut like the rest");
                }
                answer
            }
            "duplicate" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                let placed = ["to", "track", "source_in", "for"]
                    .iter()
                    .any(|k| request.get(*k).is_some());
                if !placed {
                    let copies = self.edit(cx, |p| p.duplicate(&ids));
                    let clips: Vec<Value> = copies.iter().map(|id| self.clip_json(*id)).collect();
                    return Some(self.edited_with(json!({ "new": clips })));
                }
                // Made where it is wanted, in one step.
                let [id] = ids[..] else {
                    return Some(fail(
                        "--to, --track, --source-in and --for place one copy: name one clip",
                    ));
                };
                let slot = self.doc.project.slot(id).expect("named clips exist");
                let original = self.doc.project.clip(id).expect("named clips exist");
                let (original_end, original_length) = (original.end(), original.duration);
                let to = match self.time(request, "to") {
                    Ok(t) => t.unwrap_or(original_end),
                    Err(e) => return Some(fail(e)),
                };
                let track = self.track(request).unwrap_or(slot.track);
                let source_in = match self.time(request, "source_in") {
                    Ok(t) => t,
                    Err(e) => return Some(fail(e)),
                };
                let duration = request.get("for").and_then(Value::as_f64).map(from_secs);
                if duration.is_some_and(|d| d <= 0) {
                    return Some(fail("--for is a length in seconds"));
                }
                let lengths = self.lengths();
                let made = self.edit(cx, |p| p.duplicate_to(id, to, track, source_in, duration));
                let copy = match made {
                    Ok(copy) => copy,
                    Err(e) => return Some(fail(format!("cannot copy clip {} like that: {e}", id.0))),
                };
                let want = duration.unwrap_or(original_length);
                let mut reply = self.edited_with(json!({ "new": [self.clip_json(copy)] }));
                let was = lengths.into_iter().chain([(copy, want)]).collect::<Vec<_>>();
                self.say_how_placed(&mut reply, &was, &[(copy, track)]);
                reply
            }
            "volume" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                let Some(volume) = request.get("value").and_then(Value::as_f64) else {
                    return Some(fail("`value` is missing: 1 is as recorded, 0 silent"));
                };
                self.edit(cx, |p| p.set_volume(&ids, volume as f32));
                self.edited()
            }
            "clean" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                let project = &self.doc.project;
                if let Some(id) = ids.iter().find(|id| {
                    project
                        .clip(**id)
                        .is_none_or(|c| c.generator.is_some() || project.media(c.media).is_none_or(|m| !m.has_audio))
                }) {
                    return Some(fail(format!("clip {} has no sound to clean", id.0)));
                }
                let off = flag("off") == Some(true);
                let noise = request
                    .get("noise")
                    .and_then(Value::as_f64)
                    .map(|v| v.clamp(0.0, 1.0) as f32);
                let even = flag("even");
                self.edit(cx, |p| {
                    for id in &ids {
                        if let Some(c) = p.clip_mut(*id) {
                            c.clean = if off {
                                None
                            } else {
                                let mut clean = c.clean.unwrap_or_default();
                                clean.noise = noise.unwrap_or(clean.noise);
                                clean.even = even.unwrap_or(clean.even);
                                Some(clean)
                            };
                        }
                    }
                });
                self.edited()
            }
            "track" => {
                let Some(track) = self.track(request).filter(|t| *t < self.doc.project.tracks.len()) else {
                    return Some(fail(format!(
                        "name a track from 1 to {} with `track`",
                        self.doc.project.tracks.len()
                    )));
                };
                let (muted, hidden) = (flag("muted"), flag("hidden"));
                self.edit(cx, |p| {
                    let t = &mut p.tracks[track];
                    t.muted = muted.unwrap_or(t.muted);
                    t.hidden = hidden.unwrap_or(t.hidden);
                });
                self.edited()
            }
            "text" | "color" => {
                let generator = if cmd == "text" {
                    let mut title = splice_core::model::Title {
                        text: text("text").unwrap_or("Title").to_string(),
                        ..Default::default()
                    };
                    if let Err(e) = self.apply_style(&mut title, request) {
                        return Some(fail(e));
                    }
                    Generator::Text(title)
                } else {
                    match text("color").map(parse_color) {
                        Some(Some(color)) => Generator::Color { color },
                        _ => {
                            return Some(fail(
                                "`color` is #rrggbb, #rrggbbaa or a name (black, white, paper, ink…)",
                            ));
                        }
                    }
                };
                let at = match self.time(request, "at") {
                    Ok(t) => t.unwrap_or(self.player.position),
                    Err(e) => return Some(fail(e)),
                };
                let duration = match self.time(request, "duration") {
                    Ok(d) => d.unwrap_or(3 * SECOND),
                    Err(e) => return Some(fail(e)),
                };
                let track = self.track(request).unwrap_or_else(|| {
                    // Over whatever is there.
                    self.doc
                        .project
                        .tracks
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.clips.iter().any(|c| c.start < at + duration && c.end() > at))
                        .map(|(i, _)| i + 1)
                        .max()
                        .unwrap_or(0)
                });
                let mut look = splice_core::look::Look::default();
                // `--anchor`: --x and --y place that edge or corner of it.
                let anchored = match Anchor::of(request) {
                    Ok(Some(anchor)) => {
                        let scale = request.get("scale").and_then(Value::as_f64).unwrap_or(1.0) as f32;
                        let half = self.half_extent(&generator, None, scale);
                        Some(anchor.to_centre(request, half, true))
                    }
                    Ok(None) => None,
                    Err(e) => return Some(fail(e)),
                };
                let request = anchored.as_ref().unwrap_or(request);
                if let Err(e) = apply_look(&mut look, request, 0) {
                    return Some(fail(e));
                }
                if !look.grade.is_none() {
                    return Some(fail(
                        "colour (--grade, --exposure…) is for video and pictures; a title or colour clip takes its own --color",
                    ));
                }
                let id = self.edit(cx, |p| {
                    let id = p.insert_generated(generator, track, at, duration);
                    p.set_look(id, |l| *l = look);
                    id
                });
                self.edited_with(json!({ "clip": self.clip_json_with_bounds(id) }))
            }
            "style" => {
                let ids = match self.clips(request) {
                    Ok(ids) => ids,
                    Err(e) => return Some(fail(e)),
                };
                let anchor = match Anchor::of(request) {
                    Ok(anchor) => anchor,
                    Err(e) => return Some(fail(e)),
                };
                let mut titles = Vec::new();
                // `--anchor`: that edge or corner stays put as the title changes size.
                let mut shifts = Vec::new();
                for &id in &ids {
                    match self.doc.project.clip(id).and_then(|c| c.generator.clone()) {
                        Some(Generator::Text(mut title)) => {
                            let scale = self.doc.project.clip(id).map_or(1.0, |c| c.look.scale);
                            let was = anchor.map(|_| self.half_extent(&Generator::Text(title.clone()), None, scale));
                            if let Some(words) = text("text") {
                                title.text = words.to_string();
                            }
                            if let Err(e) = self.apply_style(&mut title, request) {
                                return Some(fail(e));
                            }
                            let generator = Generator::Text(title);
                            if let (Some(anchor), Some(was)) = (anchor, was) {
                                let now = self.half_extent(&generator, None, scale);
                                shifts.push((id, anchor.shift((now.0 - was.0, now.1 - was.1))));
                            }
                            titles.push((id, generator));
                        }
                        Some(Generator::Color { .. }) => match text("color").map(parse_color) {
                            Some(Some(color)) => titles.push((id, Generator::Color { color })),
                            _ => return Some(fail("give a colour clip a new `color`")),
                        },
                        None => {
                            return Some(fail(format!(
                                "clip {} is media; `style` is for text and colour clips",
                                id.0
                            )));
                        }
                    }
                }
                self.edit(cx, |p| {
                    for (id, generator) in titles {
                        p.set_generator(id, |g| *g = generator);
                    }
                    for (id, (dx, dy)) in shifts {
                        p.set_look(id, |l| {
                            l.x += dx;
                            l.y += dy;
                            l.keys.x.iter_mut().for_each(|k| k.value += dx);
                            l.keys.y.iter_mut().for_each(|k| k.value += dy);
                        });
                    }
                });
                let clips: Vec<Value> = ids.iter().map(|id| self.clip_json_with_bounds(*id)).collect();
                self.edited_with(json!({ "clips": clips }))
            }
            "look" => {
                // `--all`: every video and picture, for one colour across the movie.
                let ids = if flag("all") == Some(true) {
                    let project = &self.doc.project;
                    project
                        .clips()
                        .filter(|(_, c)| c.generator.is_none())
                        .filter(|(_, c)| project.media(c.media).is_some_and(|m| m.is_visual()))
                        .map(|(_, c)| c.id)
                        .collect()
                } else {
                    match self.clips(request) {
                        Ok(ids) => ids,
                        Err(e) => return Some(fail(e)),
                    }
                };
                let mut looks = Vec::new();
                for &id in &ids {
                    let Some(clip) = self.doc.project.clip(id) else {
                        continue;
                    };
                    let mut look = clip.look.clone();
                    let at = (self.player.position - clip.start).clamp(0, clip.duration);
                    // `--anchor`: --x and --y place that edge or corner of it.
                    let anchored = match Anchor::of(request) {
                        Ok(Some(anchor)) => {
                            let scale = request
                                .get("scale")
                                .and_then(Value::as_f64)
                                .map_or(look.scale, |v| v as f32);
                            let half = match &clip.generator {
                                Some(generator) => self.half_extent(generator, None, scale),
                                None => self.half_extent(&Generator::Color { color: 0 }, Some(clip), scale),
                            };
                            Some(anchor.to_centre(request, half, false))
                        }
                        Ok(None) => None,
                        Err(e) => return Some(fail(e)),
                    };
                    if let Err(e) = apply_look(&mut look, anchored.as_ref().unwrap_or(request), at) {
                        return Some(fail(e));
                    }
                    if look.key.is_some() && clip.generator.is_some() {
                        return Some(fail("a green-screen key is for video and pictures, not text or colour"));
                    }
                    let pictures_only = look.backdrop != clip.look.backdrop
                        || look.mirror != clip.look.mirror
                        || look.redact != clip.look.redact
                        || look.fx.blur != clip.look.fx.blur
                        || look.fx.vignette != clip.look.fx.vignette
                        || look.fx.grain != clip.look.fx.grain;
                    if pictures_only && clip.generator.is_some() {
                        return Some(fail(
                            "blur, vignette, grain, --mirror, --backdrop and --redact are for video and pictures; a title or colour \
                             takes --shake, --glitch and the rest",
                        ));
                    }
                    if look.grade != clip.look.grade && clip.generator.is_some() {
                        return Some(fail(
                            "colour (--grade, --exposure…) is for video and pictures; a title or colour clip takes its own --color",
                        ));
                    }
                    looks.push((id, look));
                }
                self.edit(cx, |p| {
                    for (id, look) in looks {
                        p.set_look(id, |l| *l = look);
                    }
                });
                let clips: Vec<Value> = ids.iter().map(|id| self.clip_json_with_bounds(*id)).collect();
                self.edited_with(json!({ "clips": clips }))
            }
            "fonts" => {
                let query = text("query").unwrap_or("");
                let fonts = Self::font_matches(&titles::families(), query);
                json!({ "ok": true, "count": fonts.len(), "fonts": fonts })
            }
            "canvas" => {
                let mut settings = self.doc.project.settings;
                if let Some(aspect) = text("aspect") {
                    match ASPECTS.iter().find(|(name, _, _)| *name == aspect) {
                        Some((_, w, h)) => (settings.width, settings.height) = (*w, *h),
                        None => {
                            let names: Vec<&str> = ASPECTS.iter().map(|(n, _, _)| *n).collect();
                            return Some(fail(format!("`aspect` is one of {}", names.join(", "))));
                        }
                    }
                }
                if let Some(size) = text("size") {
                    let parsed = size
                        .split_once(['x', 'X', '×'])
                        .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)));
                    match parsed {
                        Some((w, h)) if (16..=8192).contains(&w) && (16..=8192).contains(&h) => {
                            (settings.width, settings.height) = (w & !1, h & !1)
                        }
                        _ => return Some(fail("`size` is WIDTHxHEIGHT, from 16 to 8192 each")),
                    }
                }
                if let Some(fps) = request.get("fps").and_then(Value::as_u64) {
                    if !(1..=120).contains(&fps) {
                        return Some(fail("`fps` is from 1 to 120"));
                    }
                    settings.fps = fps as u32;
                }
                // Black bars: a shape (2.39, 1.85, 2:1, 16:9) or none.
                let letterbox = match text("letterbox") {
                    None => self.doc.project.letterbox,
                    Some("none" | "off") => None,
                    Some(shape) => {
                        let parsed = match shape.split_once(':') {
                            Some((w, h)) => w
                                .trim()
                                .parse::<f32>()
                                .ok()
                                .zip(h.trim().parse::<f32>().ok())
                                .map(|(w, h)| w / h),
                            None => shape.trim().parse::<f32>().ok(),
                        };
                        match parsed.filter(|a| (0.3..=4.0).contains(a)) {
                            Some(a) => Some(a),
                            None => return Some(fail("`letterbox` is a shape like 2.39, 1.85 or 2:1, or none")),
                        }
                    }
                };
                self.edit(cx, |p| {
                    p.settings = settings;
                    p.letterbox = letterbox;
                });
                self.edited()
            }
            "undo" | "redo" => {
                let changed = if cmd == "undo" {
                    self.doc.undo()
                } else {
                    self.doc.redo()
                };
                self.after_edit();
                if changed {
                    self.edited()
                } else {
                    fail(format!("nothing to {cmd}"))
                }
            }
            "seek" => match self.time(request, "at") {
                Ok(Some(t)) => {
                    self.seek(t, cx);
                    self.follow_playhead();
                    json!({ "ok": true, "playhead": s(self.player.position) })
                }
                Ok(None) => fail("`at` is missing"),
                Err(e) => fail(e),
            },
            "play" | "pause" => {
                if self.player.is_playing() != (cmd == "play") {
                    self.play_pause(&PlayPause, window, cx);
                }
                json!({ "ok": true, "playing": self.player.is_playing(), "playhead": s(self.player.position) })
            }
            "select" => {
                let ids = if request.get("clips").is_some() {
                    match self.clips(request) {
                        Ok(ids) => ids,
                        Err(e) => return Some(fail(e)),
                    }
                } else {
                    Vec::new()
                };
                self.selection = ids;
                json!({ "ok": true, "selection": self.selection.iter().map(|c| c.0).collect::<Vec<_>>() })
            }
            "command" => {
                let words = text("command").unwrap_or("");
                let found = Self::palette_matches(words).into_iter().next();
                match found {
                    Some((label, _, action)) if !words.trim().is_empty() => {
                        window.dispatch_action(action, cx);
                        json!({ "ok": true, "ran": label })
                    }
                    _ => fail(format!(
                        "no command matches `{words}`; the command menu (Cmd-/) lists them"
                    )),
                }
            }
            "save" => match self.doc.save(text("label")) {
                Ok(()) => {
                    self.say("Saved a version");
                    json!({ "ok": true, "path": self.doc.path() })
                }
                Err(e) => fail(format!("save failed: {e}")),
            },
            "frame" => {
                let Some(path) = text("path").map(PathBuf::from) else {
                    return Some(fail("`path` is missing"));
                };
                let at = match self.time(request, "at") {
                    Ok(t) => t.unwrap_or(self.player.position),
                    Err(e) => return Some(fail(e)),
                };
                let plan = export::plan_still(&self.doc.project, &self.resolver(), at, &path);
                let plan = match plan {
                    Ok(plan) => plan,
                    Err(e) => return Some(fail(e)),
                };
                let (w, h) = (self.doc.project.settings.width, self.doc.project.settings.height);
                cx.background_spawn(async move {
                    let result = ffmpeg::export_command()
                        .map_err(|e| e.to_string())
                        .and_then(|mut command| {
                            let out = command.args(&plan.args).output().map_err(|e| e.to_string())?;
                            if out.status.success() {
                                Ok(())
                            } else {
                                let err = String::from_utf8_lossy(&out.stderr);
                                Err(ffmpeg::first_error(&err, "ffmpeg failed"))
                            }
                        });
                    let answer = match result {
                        Ok(()) => json!({ "ok": true, "path": path, "at": s(at), "width": w, "height": h }),
                        Err(e) => fail(e),
                    };
                    let _ = reply.send(answer.to_string());
                })
                .detach();
                return None;
            }
            "export" => {
                let Some(path) = text("path").map(PathBuf::from) else {
                    return Some(fail("`path` is missing"));
                };
                if self.export.is_some() {
                    return Some(fail("an export is already running"));
                }
                let quality = match text("quality") {
                    Some("draft") => Quality::Draft,
                    Some("high") => Quality::High,
                    _ => Quality::Standard,
                };
                // Just a stretch: --from/--to, a clip's span, or a beat.
                let range = if let Ok(Some(clips)) = self.clips_of(request).map(|c| c.first().copied()) {
                    self.doc.project.clip(clips).map(|c| (c.start, c.end()))
                } else if let Some(beat) = request.get("beat").and_then(Value::as_u64) {
                    match self.doc.project.story.span(splice_core::story::BeatId(beat)) {
                        Some(span) => Some(span),
                        None => return Some(fail(format!("no beat {beat}"))),
                    }
                } else {
                    match (self.time(request, "from"), self.time(request, "to")) {
                        (Ok(None), Ok(None)) => None,
                        (Ok(a), Ok(b)) => {
                            let (a, b) = (a.unwrap_or(0), b.unwrap_or(self.doc.project.duration()));
                            if b <= a {
                                return Some(fail("--to must be after --from"));
                            }
                            Some((a, b))
                        }
                        (Err(e), _) | (_, Err(e)) => return Some(fail(e)),
                    }
                };
                if let Err(e) = self.start_export(path.clone(), quality, false, range, cx) {
                    return Some(fail(e));
                }
                if flag("wait") == Some(true) {
                    self.export_replies.push(reply);
                    return None;
                }
                json!({ "ok": true, "path": path, "started": true })
            }
            "say" => match text("text").map(str::trim).filter(|t| !t.is_empty()) {
                Some(said) => {
                    let name = text("name").or(self.agent_name()).unwrap_or("Claude").to_string();
                    self.say(format!("{name}: {}", said.lines().next().unwrap_or("")));
                    json!({ "ok": true })
                }
                None => fail("`text` is missing"),
            },
            "wait" => {
                let name = text("name")
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or("Claude")
                    .to_string();
                self.agent.working = None;
                match self.agent.outbox.pop_front() {
                    Some(message) => {
                        let _ = reply.send(message);
                        self.agent.working = Some(name);
                    }
                    None => self.agent.waiters.push_back(Waiter { id, name, reply }),
                }
                return None;
            }
            other => fail(format!("unknown command `{other}`; `splice ctl --help` lists them")),
        };
        Some(answer)
    }

    /// Text style fields from a request, onto `title`.
    fn apply_style(&self, title: &mut splice_core::model::Title, request: &Value) -> Result<(), String> {
        let text = |key: &str| request.get(key).and_then(Value::as_str);
        let flag = |key: &str| request.get(key).and_then(Value::as_bool);
        if let Some(font) = text("font") {
            let families = titles::families();
            match families.iter().find(|f| f.eq_ignore_ascii_case(font)) {
                Some(found) => title.font = found.clone(),
                None => {
                    return Err(format!(
                        "no font named `{font}` here; `splice ctl fonts --query {font}` searches"
                    ));
                }
            }
        }
        if let Some(size) = request.get("size").and_then(Value::as_f64) {
            // Pixels on the canvas, or a fraction of its height.
            let h = self.doc.project.settings.height as f64;
            title.size = if size > 1.0 { size / h } else { size }.clamp(0.005, 0.6) as f32;
        }
        if let Some(c) = text("color") {
            title.color = parse_color(c).ok_or(format!("`color` {c} is not #rrggbb or a name"))?;
        }
        match text("box") {
            Some("none") => title.background = None,
            Some(c) => title.background = Some(parse_color(c).ok_or(format!("`box` {c} is not #rrggbbaa or a name"))?),
            None => {}
        }
        if let Some(align) = text("align") {
            title.align = match align {
                "left" => splice_core::model::Align::Left,
                "center" | "centre" => splice_core::model::Align::Center,
                "right" => splice_core::model::Align::Right,
                _ => return Err("`align` is left, center or right".into()),
            };
        }
        title.bold = flag("bold").unwrap_or(title.bold);
        title.italic = flag("italic").unwrap_or(title.italic);
        title.shadow = flag("shadow").unwrap_or(title.shadow);
        // Coming in a piece at a time.
        use splice_core::textmotion::{By, Style, TextMotion};
        match request.get("animate").and_then(Value::as_str) {
            Some("none" | "off") => title.animate = None,
            Some(name) => {
                let style = Style::parse(name).ok_or_else(|| {
                    let names: Vec<_> = Style::ALL.iter().map(|s| s.name()).collect();
                    format!("`animate` is none or one of {}", names.join(", "))
                })?;
                title.animate = Some(TextMotion {
                    style,
                    ..title.animate.unwrap_or_default()
                });
            }
            None => {}
        }
        if let Some(m) = &mut title.animate {
            if let Some(name) = request.get("by").and_then(Value::as_str) {
                m.by = By::parse(name).ok_or("`by` is word, letter or line")?;
            }
            if let Some(over) = request.get("over").and_then(Value::as_f64) {
                m.over = splice_core::time::from_secs(over.clamp(0.05, 30.0));
            }
        }
        Ok(())
    }

    fn agent_name(&self) -> Option<&str> {
        self.agent
            .working
            .as_deref()
            .or_else(|| self.agent.waiters.front().map(|w| w.name.as_str()))
    }

    /// Hands the person's message to the assistant, or queues it while the
    /// assistant is busy with the last one.
    pub(super) fn send_to_agent(&mut self, text: String, cx: &mut Context<Self>) {
        let selection: Vec<Value> = self.selection.iter().map(|id| self.clip_json(*id)).collect();
        let message = json!({
            "ok": true,
            "message": text,
            "project": self.doc.path(),
            "revision": self.doc.revision,
            "playhead": s(self.player.position),
            "selection": selection,
            "media_selected": self.bin_selection.and_then(|id| self.doc.project.media(id)).map(|m| json!({ "id": m.id.0, "name": m.name })),
            "editing": self.workbench.as_ref().map(|b| self.editing_json(b.media)),
        })
        .to_string();
        let mut message = Some(message);
        while let (Some(waiter), Some(text)) = (self.agent.waiters.pop_front(), message.take()) {
            match waiter.reply.send(text) {
                Ok(()) => self.agent.working = Some(waiter.name),
                Err(unsent) => message = Some(unsent.0),
            }
        }
        match (message, self.agent.working.clone()) {
            (None, Some(name)) => self.say(format!("Sent to {name}")),
            (Some(message), Some(name)) => {
                self.agent.outbox.push_back(message);
                self.say(format!("{name} is busy; your message is queued"));
            }
            _ => self.say_error("No assistant is connected — one connects with `splice ctl wait`"),
        }
        cx.notify();
    }

    fn edited(&self) -> Value {
        self.edited_with(json!({}))
    }

    pub(super) fn edited_with(&self, mut extra: Value) -> Value {
        let base = json!({
            "ok": true,
            "revision": self.doc.revision,
            "duration": s(self.doc.project.duration()),
            "tracks": self.doc.project.tracks.len(),
        });
        if let (Some(extra), Value::Object(base)) = (extra.as_object_mut(), base) {
            for (k, v) in base {
                extra.entry(k).or_insert(v);
            }
        }
        extra
    }

    /// Half the size of what is seen of a picture at `scale`, as fractions of
    /// the canvas: a title or colour as drawn (a title without the clear
    /// room around its lines), or `media`'s clip fitted to the canvas.
    fn half_extent(&self, generator: &Generator, media: Option<&splice_core::model::Clip>, scale: f32) -> (f32, f32) {
        let project = &self.doc.project;
        let canvas = (project.settings.width, project.settings.height);
        let (w, h, inset) = match (media, generator) {
            (Some(clip), _) => {
                let size = project
                    .source(clip)
                    .map(|source| splice_core::export::natural_size(source, None, canvas.0, canvas.1));
                let (w, h) = size.unwrap_or((canvas.0 as f64, canvas.1 as f64));
                (w as f32, h as f32, 0.0)
            }
            (None, Generator::Text(title)) => titles::measure(title, canvas),
            (None, Generator::Color { .. }) => (canvas.0 as f32, canvas.1 as f32, 0.0),
        };
        (
            (w / 2.0 - inset).max(0.0) * scale / canvas.0 as f32,
            (h / 2.0 - inset).max(0.0) * scale / canvas.1 as f32,
        )
    }

    /// A clip for a reply, with where its edges are on the canvas at its
    /// start (`bounds`, in the units of --x and --y: 0 is the centre, ±0.5
    /// the canvas's edges), so a script can lay things out against it.
    fn clip_json_with_bounds(&self, id: ClipId) -> Value {
        let mut value = self.clip_json(id);
        if let Some(clip) = self.doc.project.clip(id) {
            let look = &clip.look;
            let (hw, hh) = match &clip.generator {
                Some(generator) => self.half_extent(generator, None, look.scale),
                None => self.half_extent(&Generator::Color { color: 0 }, Some(clip), look.scale),
            };
            let r = |v: f32| (v as f64 * 10000.0).round() / 10000.0;
            value["bounds"] = json!({
                "left": r(look.x - hw), "right": r(look.x + hw),
                "top": r(look.y - hh), "bottom": r(look.y + hh),
                "width": r(hw * 2.0), "height": r(hh * 2.0),
            });
        }
        value
    }

    /// Where a trimmed edge stopped short of `asked` (more than half a frame
    /// off), and what stopped it: the clip beside it, the media's own start
    /// or end, or the shortest a clip can be.
    fn trim_stopped(&self, id: ClipId, edge: &str, asked: Time) -> Option<Value> {
        let project = &self.doc.project;
        let frame = project.frame();
        let slot = project.slot(id)?;
        let clips = &project.tracks[slot.track].clips;
        let clip = &clips[slot.index];
        let got = if edge == "start" { clip.start } else { clip.end() };
        if (got - asked).abs() <= frame / 2 {
            return None;
        }
        let by = if edge == "start" {
            let before = slot.index.checked_sub(1).map(|i| &clips[i]);
            match before {
                _ if got >= clip.end() - frame => "the shortest a clip can be (a frame)".to_string(),
                Some(b) if b.end() == got => format!("clip {} before it on the track", b.id.0),
                _ if got == 0 => "the start of the timeline".to_string(),
                _ => "the start of its media".to_string(),
            }
        } else {
            match clips.get(slot.index + 1) {
                _ if got <= clip.start + frame => "the shortest a clip can be (a frame)".to_string(),
                Some(n) if n.start == got => format!("clip {} after it on the track", n.id.0),
                _ => "the end of its media".to_string(),
            }
        };
        Some(json!({ "clip": id.0, "edge": edge, "asked": s(asked), "got": s(got), "by": by }))
    }

    /// How long every clip is, to tell afterwards which an edit cut short.
    fn lengths(&self) -> Vec<(ClipId, Time)> {
        self.doc.project.clips().map(|(_, c)| (c.id, c.duration)).collect()
    }

    /// Adds to a reply what placing clips did besides putting them where
    /// asked: `trimmed`, the clips cut to meet a neighbour they overlapped
    /// by a frame or less (`was` is how long each was, or would have been),
    /// and `rose`, the clips that found their track (`asked`) taken and went
    /// up to another, where they cover the layers between.
    fn say_how_placed(&self, reply: &mut Value, was: &[(ClipId, Time)], asked: &[(ClipId, usize)]) {
        let project = &self.doc.project;
        let trimmed: Vec<Value> = was
            .iter()
            .filter_map(|(id, was)| {
                let clip = project.clip(*id)?;
                (clip.duration < *was)
                    .then(|| json!({ "clip": id.0, "by": s(was - clip.duration), "end": s(clip.end()) }))
            })
            .collect();
        let rose: Vec<Value> = asked
            .iter()
            .filter_map(|(id, asked)| {
                let track = project.slot(*id)?.track;
                (track > *asked && *asked < project.tracks.len())
                    .then(|| json!({ "clip": id.0, "asked": asked + 1, "track": track + 1 }))
            })
            .collect();
        let mut notes = Vec::new();
        if !trimmed.is_empty() {
            notes.push("a clip overlapped its neighbour by a frame or less: the earlier one now ends at the cut");
            reply["trimmed"] = json!(trimmed);
        }
        if !rose.is_empty() {
            notes.push("the track asked for was taken there: the clip is on a track above, over the layers between");
            reply["rose"] = json!(rose);
        }
        if !notes.is_empty() {
            reply["note"] = json!(notes.join("; "));
            // One field a script can check after every edit.
            reply["warnings"] = json!(notes);
        }
    }

    /// A clip as `ctl` shows it: times in seconds, its track counted from 1.
    pub(super) fn clip_json(&self, id: ClipId) -> Value {
        let project = &self.doc.project;
        let (Some(slot), Some(clip)) = (project.slot(id), project.clip(id)) else {
            return json!({ "id": id.0, "gone": true });
        };
        let kind = match &clip.generator {
            Some(Generator::Text(_)) => "text",
            Some(Generator::Color { .. }) => "color",
            None => project.media(clip.media).map_or("missing", |m| kind(m.kind)),
        };
        let mut value = json!({
            "id": clip.id.0,
            "name": project.clip_name(clip),
            "kind": kind,
            "track": slot.track + 1,
            "start": s(clip.start),
            "end": s(clip.end()),
            "duration": s(clip.duration),
        });
        if let Some(label) = &clip.placeholder {
            value["placeholder"] = json!(label);
        }
        match &clip.generator {
            None => {
                value["media"] = json!(clip.media.0);
                value["source_in"] = json!(s(clip.source_in));
                value["volume"] = json!(clip.volume);
                if let Some(clean) = clip.clean {
                    value["clean"] = json!(clean);
                }
            }
            Some(Generator::Text(title)) => {
                let h = project.settings.height as f32;
                value["text"] = json!({
                    "text": title.text,
                    "font": title.font,
                    "size": (title.size * h).round(),
                    "bold": title.bold,
                    "italic": title.italic,
                    "color": hex(title.color),
                    "align": title.align,
                    "box": title.background.map(hex),
                    "shadow": title.shadow,
                    "animate": title.animate.map(|m| json!({
                        "style": m.style.name(),
                        "by": m.by.name(),
                        "over": s(m.over),
                    })),
                    "timed": !title.timing.is_empty(),
                });
            }
            Some(Generator::Color { color }) => value["color"] = json!(hex(*color)),
        }
        if clip.look != splice_core::look::Look::default() {
            value["look"] = look_json(&clip.look);
        }
        if clip.frozen {
            value["frozen"] = json!(true);
        }
        if let Some(m) = project
            .media(clip.media)
            .filter(|m| !m.clicks.is_empty() && clip.generator.is_none())
        {
            value["clicks"] = json!(m.clicks.len());
        }
        if clip.speed != 1.0 {
            value["speed"] = json!(clip.speed);
        }
        if clip.reverse {
            value["reverse"] = json!(true);
        }
        if let Some(cut) = project.cut_into(clip.id) {
            value["transition"] = json!({
                "kind": cut.kind.name(),
                "duration": s(cut.duration()),
                "from": cut.from.map(|c| c.0),
                "start": s(cut.start),
                "end": s(cut.end),
            });
        }
        value
    }

    fn ctl_status(&self) -> Value {
        let project = &self.doc.project;
        let settings = project.settings;
        let export = self.export.as_ref().map(|job| {
            let job = job.lock().unwrap();
            // How long it has run, and at this rate how long is left.
            let elapsed = job.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
            let left =
                (job.progress > 0.02).then(|| (elapsed * (1.0 - job.progress as f64) / job.progress as f64).round());
            json!({
                "path": job.output,
                "progress": (job.progress * 100.0).round() / 100.0,
                "elapsed": elapsed.round(),
                "left": left,
                "width": job.size.0,
                "height": job.size.1,
            })
        });
        json!({
            "ok": true,
            "project": {
                "name": self.doc.name(),
                "path": self.doc.path(),
                "untitled": self.doc.is_untitled(),
                "saved": !self.doc.edited,
                "revision": self.doc.revision,
            },
            "canvas": {
                "width": settings.width,
                "height": settings.height,
                "fps": settings.fps,
                "letterbox": self.doc.project.letterbox,
            },
            "duration": s(project.duration()),
            "playhead": s(self.player.position),
            "playing": self.player.is_playing(),
            "playback": self.player.keeping,
            "selection": self.selection.iter().map(|c| c.0).collect::<Vec<_>>(),
            "media": project.media.len(),
            "tracks": project.tracks.len(),
            "clips": project.clips().count(),
            "preparing": self.working > 0,
            "export": export,
        })
    }

    fn ctl_read(&self) -> Value {
        let project = &self.doc.project;
        let media: Vec<Value> = project
            .media
            .iter()
            .map(|m| {
                let assets = self.library.get(&m.hash);
                json!({
                    "id": m.id.0,
                    "name": m.name,
                    "kind": kind(m.kind),
                    "duration": m.is_timed().then(|| s(m.duration)),
                    "width": m.is_visual().then_some(m.width),
                    "height": m.is_visual().then_some(m.height),
                    "audio": m.has_audio,
                    "file": assets.and_then(|a| a.path.as_ref()),
                    "ready": assets.is_some_and(|a| a.path.is_some() && !a.loading),
                    "made": m.recipe.as_ref().map(|r| r.kind()),
                    "rendering": self.render_of(m.id).map(|r| r.progress()),
                    "render_error": self.made.errors.get(&m.id),
                })
            })
            .collect();
        let tracks: Vec<Value> = project
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "track": i + 1,
                    "muted": t.muted,
                    "hidden": t.hidden,
                    "clips": t.clips.iter().map(|c| self.clip_json(c.id)).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut status = self.ctl_status();
        status["media"] = json!(media);
        status["tracks"] = json!(tracks);
        status.as_object_mut().map(|o| o.remove("clips"));
        status
    }

    /// A time field: seconds, or "end" for the end of the movie.
    pub(super) fn time(&self, request: &Value, key: &str) -> Result<Option<Time>, String> {
        match request.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(word)) if word == "end" => Ok(Some(self.doc.project.duration())),
            Some(Value::String(text)) => crate::ctl::parse_time(text)
                .map(|t| Some(from_secs(t)))
                .ok_or(format!("`{key}` is a time in seconds")),
            Some(v) => v
                .as_f64()
                .filter(|t| *t >= 0.0)
                .map(|t| Some(from_secs(t)))
                .ok_or(format!("`{key}` is a time in seconds")),
        }
    }

    /// The track named in a request, as an index (requests count from 1).
    pub(super) fn track(&self, request: &Value) -> Option<usize> {
        request
            .get("track")
            .and_then(Value::as_u64)
            .map(|t| (t as usize).saturating_sub(1))
    }

    fn clips(&self, request: &Value) -> Result<Vec<ClipId>, String> {
        let ids: Vec<ClipId> = match &request["clips"] {
            Value::Array(ids) => ids.iter().filter_map(Value::as_u64).map(ClipId).collect(),
            Value::Number(id) => id.as_u64().map(ClipId).into_iter().collect(),
            _ => Vec::new(),
        };
        if ids.is_empty() {
            return Err("name clips with `clips` (ids from `splice ctl read`)".into());
        }
        match ids.iter().find(|id| self.doc.project.slot(**id).is_none()) {
            Some(missing) => Err(format!("there is no clip {}", missing.0)),
            None => Ok(ids),
        }
    }

    /// The clips a request names, or none when it names none.
    pub(super) fn clips_of(&self, request: &Value) -> Result<Vec<ClipId>, String> {
        if request.get("clips").is_none() {
            return Ok(Vec::new());
        }
        self.clips(request)
    }

    pub(super) fn find_media(&self, spec: &Value) -> Result<MediaId, String> {
        let media = &self.doc.project.media;
        let found = match spec {
            Value::Number(id) => media.iter().find(|m| Some(m.id.0) == id.as_u64()),
            Value::String(name) => media.iter().find(|m| m.name == *name).or_else(|| {
                let lower = name.to_lowercase();
                let mut close = media.iter().filter(|m| m.name.to_lowercase().contains(&lower));
                match (close.next(), close.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            }),
            _ => return Err("name the media with `media`: its id or file name".into()),
        };
        found
            .map(|m| m.id)
            .ok_or(format!("no media matches {spec}; `splice ctl read` lists it"))
    }
}

/// Look fields from a request, onto `look`.
/// `at` is clip time, where a keyframed value is keyed.
/// The point of a picture that `--x` and `--y` place: its centre, an edge
/// or a corner. Each is -1 (left, top), 0 (the middle) or 1 (right, bottom).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Anchor {
    across: f32,
    down: f32,
}

impl Anchor {
    /// The request's `anchor`: left, right, top, bottom, center, or a
    /// corner as two of them (top-left, bottom-right).
    pub(super) fn of(request: &Value) -> Result<Option<Anchor>, String> {
        let Some(words) = request.get("anchor").and_then(Value::as_str) else {
            return Ok(None);
        };
        let mut anchor = Anchor { across: 0.0, down: 0.0 };
        for word in words.split(['-', ' ', '_']).filter(|w| !w.is_empty()) {
            match word {
                "left" => anchor.across = -1.0,
                "right" => anchor.across = 1.0,
                "top" => anchor.down = -1.0,
                "bottom" => anchor.down = 1.0,
                "center" | "centre" | "middle" => {}
                _ => {
                    return Err(
                        "`anchor` is left, right, top, bottom, center, or a corner such as top-left or bottom-right"
                            .into(),
                    );
                }
            }
        }
        Ok(Some(anchor))
    }

    /// How far the centre moves to keep this point still when the picture's
    /// half size grows by `grown`.
    fn shift(self, grown: (f32, f32)) -> (f32, f32) {
        (-self.across * grown.0, -self.down * grown.1)
    }

    /// The request with `x` and `y` naming the picture's centre, as looks
    /// hold them, for a picture of half size `half`. A value not given is
    /// left out (`or_zero`: taken as 0, the canvas's centre, for a new clip).
    fn to_centre(self, request: &Value, half: (f32, f32), or_zero: bool) -> Value {
        let mut out = request.clone();
        for (key, toward, half) in [("x", self.across, half.0), ("y", self.down, half.1)] {
            let given = request.get(key).and_then(Value::as_f64);
            if let Some(v) = given.or(or_zero.then_some(0.0)) {
                out[key] = json!(v - (toward * half) as f64);
            }
        }
        out
    }
}

pub(super) fn apply_look(look: &mut splice_core::look::Look, request: &Value, at: Time) -> Result<(), String> {
    use splice_core::look::{Drift, Key, Mask, MotionKind, Shape};
    let text = |key: &str| request.get(key).and_then(Value::as_str);
    let number = |key: &str| request.get(key).and_then(Value::as_f64).map(|v| v as f32);
    let flag = |key: &str| request.get(key).and_then(Value::as_bool);
    // A keyframed value takes a keyframe at `at`; the rest change throughout.
    use splice_core::look::Prop;
    for prop in Prop::ALL {
        if let Some(v) = number(prop.name()) {
            look.set_value(prop, at, v);
        }
    }
    for (key, motion) in [("enter", &mut look.enter), ("exit", &mut look.exit)] {
        if let Some(kind) = text(key) {
            motion.kind = MotionKind::parse(kind).ok_or_else(|| {
                let names: Vec<_> = MotionKind::ALL.iter().map(|k| k.name()).collect();
                format!("`{key}` is one of {}", names.join(", "))
            })?;
        }
        if let Some(d) = number(&format!("{key}_for")) {
            motion.duration = from_secs(d.clamp(0.05, 10.0) as f64);
        }
    }
    if let Some(drift) = text("drift") {
        look.drift = match drift {
            "none" => Drift::None,
            "zoom-in" => Drift::ZoomIn,
            "zoom-out" => Drift::ZoomOut,
            _ => return Err("`drift` is none, zoom-in or zoom-out".into()),
        };
    }
    match text("key") {
        Some("none") => look.key = None,
        Some(color) => {
            let rgba = parse_color(color).ok_or(format!("`key` {color} is green, blue, #rrggbb or none"))?;
            look.key = Some(Key {
                color: rgba >> 8,
                ..look.key.unwrap_or_default()
            });
        }
        None => {}
    }
    if let Some(key) = &mut look.key {
        key.similarity = number("similarity").map_or(key.similarity, |v| v.clamp(0.01, 0.6));
        key.blend = number("blend").map_or(key.blend, |v| v.clamp(0.0, 0.5));
        key.despill = flag("despill").unwrap_or(key.despill);
    }
    match text("mask") {
        Some("none") => look.mask = None,
        Some(shape) => {
            let shape = match shape {
                "ellipse" | "circle" => Shape::Ellipse,
                "rect" | "box" => Shape::Rect,
                _ => return Err("`mask` is ellipse, rect or none".into()),
            };
            look.mask = Some(Mask {
                shape,
                ..look.mask.unwrap_or_default()
            });
        }
        None => {}
    }
    if let Some(mask) = &mut look.mask {
        mask.x = number("mask_x").unwrap_or(mask.x);
        mask.y = number("mask_y").unwrap_or(mask.y);
        mask.width = number("mask_width").map_or(mask.width, |v| v.max(0.01));
        mask.height = number("mask_height").map_or(mask.height, |v| v.max(0.01));
        mask.feather = number("feather").map_or(mask.feather, |v| v.clamp(0.001, 1.0));
        mask.invert = flag("invert").unwrap_or(mask.invert);
    }
    if let Some(name) = text("grade") {
        look.grade.preset = splice_core::look::Preset::parse(name).ok_or_else(|| {
            let names: Vec<_> = splice_core::look::Preset::ALL.iter().map(|p| p.name()).collect();
            format!("`grade` is one of {}", names.join(", "))
        })?;
    }
    let grade = &mut look.grade;
    grade.exposure = number("exposure").unwrap_or(grade.exposure);
    grade.contrast = number("contrast").unwrap_or(grade.contrast);
    grade.saturation = number("saturation").unwrap_or(grade.saturation);
    grade.temperature = number("warmth").unwrap_or(grade.temperature);
    grade.tint = number("tint").unwrap_or(grade.tint);
    *grade = grade.clamped();
    let fx = &mut look.fx;
    fx.blur = number("blur").unwrap_or(fx.blur);
    fx.vignette = number("vignette").unwrap_or(fx.vignette);
    fx.grain = number("grain").unwrap_or(fx.grain);
    fx.shake = number("shake").unwrap_or(fx.shake);
    fx.glitch = number("glitch").unwrap_or(fx.glitch);
    *fx = fx.clamped();
    look.mirror = flag("mirror").unwrap_or(look.mirror);
    match text("backdrop") {
        Some("on" | "yes" | "blur") => look.backdrop = Some(look.backdrop.unwrap_or_default()),
        Some("off" | "no" | "none") => look.backdrop = None,
        Some(other) => return Err(format!("`backdrop` is on or off, not {other}")),
        None => {}
    }
    if let Some(b) = &mut look.backdrop {
        b.blur = number("backdrop_blur").map_or(b.blur, |v| v.clamp(0.0, 1.0));
        b.dim = number("backdrop_dim").map_or(b.dim, |v| v.clamp(0.0, 1.0));
    }
    match text("redact") {
        Some("none" | "off") => look.redact = None,
        Some(name) => {
            let style = splice_core::look::RedactStyle::parse(name)
                .ok_or(format!("`redact` is blur, pixelate, black or none, not {name}"))?;
            look.redact = Some(splice_core::look::Redact {
                style,
                ..look.redact.unwrap_or_default()
            });
        }
        None => {}
    }
    if let Some(r) = &mut look.redact {
        r.x = number("redact_x").map_or(r.x, |v| v.clamp(0.0, 1.0));
        r.y = number("redact_y").map_or(r.y, |v| v.clamp(0.0, 1.0));
        r.width = number("redact_width").map_or(r.width, |v| v.clamp(0.02, 1.0));
        r.height = number("redact_height").map_or(r.height, |v| v.clamp(0.02, 1.0));
    }
    if text("crop") == Some("none") {
        look.crop = None;
    }
    let edges = ["crop_left", "crop_top", "crop_right", "crop_bottom"];
    if edges.iter().any(|k| number(k).is_some()) {
        let mut c = look.crop.unwrap_or_default();
        c.left = number("crop_left").unwrap_or(c.left);
        c.top = number("crop_top").unwrap_or(c.top);
        c.right = number("crop_right").unwrap_or(c.right);
        c.bottom = number("crop_bottom").unwrap_or(c.bottom);
        let c = c.clamped();
        look.crop = (!c.is_none()).then_some(c);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_places_an_edge_or_a_corner() {
        let of = |words: &str| Anchor::of(&json!({ "anchor": words }));
        assert_eq!(Anchor::of(&json!({})), Ok(None));
        assert!(of("sideways").is_err());
        let half = (0.2, 0.05);
        // Left edge at -0.45: the centre is half a width to its right.
        let left = of("left").unwrap().unwrap();
        let placed = left.to_centre(&json!({ "x": -0.45, "y": 0.3 }), half, false);
        assert!((placed["x"].as_f64().unwrap() + 0.25).abs() < 1e-6);
        assert!(
            (placed["y"].as_f64().unwrap() - 0.3).abs() < 1e-6,
            "not anchored down: y is the centre"
        );
        // A corner, and what is not given: left alone, or the canvas's centre for a new clip.
        let corner = of("bottom-right").unwrap().unwrap();
        let placed = corner.to_centre(&json!({ "x": 0.45 }), half, false);
        assert!((placed["x"].as_f64().unwrap() - 0.25).abs() < 1e-6);
        assert!(placed.get("y").is_none());
        let placed = corner.to_centre(&json!({}), half, true);
        assert!((placed["y"].as_f64().unwrap() + 0.05).abs() < 1e-6);
        // Growing wider by 0.1 each side: the centre moves right to keep the left edge.
        assert_eq!(left.shift((0.1, 0.02)), (0.1, 0.0));
        assert_eq!(of("center").unwrap().unwrap().shift((0.1, 0.02)), (0.0, 0.0));
    }
}
