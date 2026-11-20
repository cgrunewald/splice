//! The script in the window. A script loaded from a text file becomes story
//! beats, each with the lines said in it and directions for the cut. The
//! Record tab shows the lines on a teleprompter over the scene, rolling at a
//! reading pace while a take records (Splice keeps itself out of the shot).
//! Afterwards the take is heard and matched to each beat's lines, so the
//! reading can be cut to the story: by the person, or by an assistant
//! through `splice ctl story match` and `story cut`.

use super::render::{caps, icon, tip};
use super::story::{Mode, Target, Typing};
use super::*;
use gpui::FontWeight;
use serde_json::{Value, json};
use splice_core::model::Clip;
use splice_core::script::{self, Prompter};
use splice_core::story::{Beat, BeatId, DirectionId};
use splice_core::studio::Studio;
use std::cell::RefCell;
use std::sync::mpsc::Sender;

/// Where the reading line sits, down the scene.
const LINE: f32 = 0.34;

/// The teleprompter as it rolls.
#[derive(Default)]
pub struct Prompt {
    /// How many of the script's words have gone past the reading line.
    pub read: f32,
    /// Rolling, and when it was last moved on.
    rolling: Option<Instant>,
    /// Where the script and each beat's part of it were drawn, measured as
    /// they are painted: rolling is by words, and these say how tall they are.
    content: Rc<Cell<Bounds<Pixels>>>,
    parts: Rc<RefCell<HashMap<BeatId, Bounds<Pixels>>>>,
}

fn clock(seconds: f32) -> String {
    let s = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn rgb(color: u32) -> gpui::Hsla {
    gpui::rgb(color).into()
}

/// A small square button with a glyph.
fn nudge(id: &'static str, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(24.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .border_1()
        .border_color(Theme::edge())
        .hover(|d| d.bg(Theme::surface0()))
        .child(icon(glyph, Theme::subtext()).size(px(11.0)))
}

/// A button with words on it.
fn button(id: &'static str, label: &str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(9.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .cursor_pointer()
        .border_1()
        .border_color(Theme::edge())
        .hover(|d| d.bg(Theme::surface0()))
        .text_size(px(11.5))
        .text_color(Theme::text())
        .child(label.to_string())
}

impl Editor {
    // The script.

    /// The beats with lines to say, and how many words each has.
    fn script_parts(&self) -> Vec<(BeatId, usize)> {
        self.doc
            .project
            .story
            .beats
            .iter()
            .map(|b| (b.id, script::words(&b.script)))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// Asks for a script file and adds it to the story.
    pub(super) fn load_script(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Load script".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let read = cx
                .background_spawn(async move {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if size > 2_000_000 {
                        return Err("That file is too big to be a script".to_string());
                    }
                    std::fs::read_to_string(&path)
                        .map_err(|_| format!("{} is not plain text: save the script as .txt or .md", file_name(&path)))
                })
                .await;
            this.update(cx, |editor, cx| {
                if let Err(e) = read.and_then(|text| editor.add_script_text(&text, false, cx)) {
                    editor.say_error(e);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The clipboard's text as a script.
    fn paste_script(&mut self, cx: &mut Context<Self>) {
        match cx.read_from_clipboard().and_then(|c| c.text()) {
            Some(text) => {
                if let Err(e) = self.add_script_text(&text, false, cx) {
                    self.say_error(e);
                }
            }
            None => self.say_error("There is no text on the clipboard to paste as a script"),
        }
        cx.notify();
    }

    /// Adds a script to the story as beats after those there (or in place
    /// of them: their footage stays). One undo step.
    pub(super) fn add_script_text(
        &mut self,
        text: &str,
        replace: bool,
        cx: &mut Context<Self>,
    ) -> Result<Vec<BeatId>, String> {
        let sections = script::parse(text);
        if sections.is_empty() {
            return Err("No words found in that script".into());
        }
        self.typing = None;
        let mut studio = self.studio();
        let pace = studio.prompter.clamped().pace;
        // In Record, a script loaded is a script to read.
        let show = self.mode == Mode::Record && !studio.prompter.on;
        studio.prompter.on |= show;
        let ids = self.edit(cx, |p| {
            if replace {
                for beat in p.story.beats.iter().map(|b| b.id).collect::<Vec<_>>() {
                    p.remove_beat(beat);
                }
            }
            if show {
                p.studio = Some(studio);
            }
            p.add_script(text, pace)
        });
        self.beat = ids.first().copied();
        self.rec.prompt.read = 0.0;
        self.rec.prompt.rolling = None;
        let words: usize = sections.iter().map(|s| script::words(&s.lines)).sum();
        self.say(format!(
            "Script loaded: {} beat{}, {words} words, about {} to read",
            ids.len(),
            if ids.len() == 1 { "" } else { "s" },
            clock(words as f32 * 60.0 / pace as f32)
        ));
        Ok(ids)
    }

    /// A beat card's lines and directions.
    pub(super) fn script_rows(&mut self, beat: &Beat, cx: &mut Context<Self>) -> AnyElement {
        let id = beat.id;
        let typing = self.typing.as_ref().map(|t| t.target);
        let words = script::words(&beat.script);
        let pace = self.studio().prompter.clamped().pace;
        let directions: Vec<AnyElement> = beat
            .directions
            .iter()
            .map(|d| {
                let (did, done) = (d.id, d.done);
                let group: SharedString = format!("direction-{}", did.0).into();
                div()
                    .group(group.clone())
                    .flex()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id(("direction-done", did.0 as usize))
                            .flex_none()
                            .mt(px(2.0))
                            .cursor_pointer()
                            .tooltip(tip(
                                if done {
                                    "Done in the cut: click to reopen"
                                } else {
                                    "A direction for the cut: click when it is done"
                                },
                                None,
                            ))
                            .child(
                                icon(
                                    if done { "check" } else { "direction" },
                                    if done { Theme::wave() } else { Theme::overlay() },
                                )
                                .size(px(11.0)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.end_typing(cx);
                                this.edit(cx, |p| p.set_direction(did, |d| d.done = !d.done));
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(done && typing != Some(Target::Direction(did)), |d| d.opacity(0.5))
                            .child(self.field(Target::Direction(did), "A note for the cut", 11.0, cx)),
                    )
                    .child(
                        div()
                            .id(("direction-remove", did.0 as usize))
                            .flex_none()
                            .size(px(15.0))
                            .rounded(px(4.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .hover(|s| s.bg(Theme::surface0()))
                            .child(icon("close", Theme::overlay()).size(px(9.0)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.typing = None;
                                this.edit(cx, |p| p.remove_direction(did));
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(icon("script", Theme::overlay()).size(px(11.0)).mt(px(3.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                self.field(Target::Script(id), "What is said", 11.0, cx)
                                    // A long script shows its start until it is typed into.
                                    .when(typing != Some(Target::Script(id)), |d| {
                                        d.max_h(px(62.0)).overflow_hidden()
                                    }),
                            )
                            .when(words > 0, |d| {
                                d.child(div().text_size(px(9.5)).text_color(Theme::overlay()).child(format!(
                                    "{words} word{} · about {} to say",
                                    if words == 1 { "" } else { "s" },
                                    clock(words as f32 * 60.0 / pace as f32)
                                )))
                            }),
                    ),
            )
            .children(directions)
            .child(
                div()
                    .id(("direction-add", id.0 as usize))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(px(10.5))
                    .text_color(Theme::surface2())
                    .hover(|s| s.text_color(Theme::subtext()))
                    .tooltip(tip(
                        "A note for whoever cuts this beat: zoom in, fade out, hold on the chart",
                        None,
                    ))
                    .child(icon("direction", Theme::surface2()).size(px(11.0)))
                    .child("Add a direction")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.end_typing(cx);
                        // The direction and its words are one undo step; left
                        // empty, it goes again.
                        let before = this.doc.project.clone();
                        if let Some(direction) = this.doc.project.add_direction(id, "") {
                            this.typing = Some(Typing {
                                target: Target::Direction(direction),
                                before,
                                cursor: 0,
                                all: false,
                            });
                            this.beat = Some(id);
                            window.focus(&this.story_focus);
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    // The teleprompter.

    fn set_prompter(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Prompter)) {
        self.set_studio(cx, |s| {
            f(&mut s.prompter);
            s.prompter = s.prompter.clamped();
        });
    }

    /// Whether the teleprompter is up with something to read.
    pub(super) fn prompting(&self) -> bool {
        self.mode == Mode::Record
            && self.workbench.is_none()
            && self.studio().prompter.on
            && self.doc.project.has_script()
    }

    /// Starts or stops the script rolling (None: the other of what it is
    /// doing). At the end, it starts again from the top.
    pub(super) fn roll_prompter(&mut self, roll: Option<bool>, cx: &mut Context<Self>) {
        let roll = roll.unwrap_or(self.rec.prompt.rolling.is_none()) && self.prompting();
        if roll {
            let total: usize = self.script_parts().iter().map(|(_, n)| n).sum();
            if self.rec.prompt.read >= total as f32 - 0.5 {
                self.rec.prompt.read = 0.0;
            }
            self.rec.prompt.rolling = Some(Instant::now());
        } else {
            self.rec.prompt.rolling = None;
        }
        cx.notify();
    }

    /// Moves the script on by the time since it last moved. True while it rolls.
    pub(super) fn tick_prompter(&mut self) -> bool {
        let Some(last) = self.rec.prompt.rolling else {
            return false;
        };
        if !self.prompting() {
            self.rec.prompt.rolling = None;
            return false;
        }
        let total: usize = self.script_parts().iter().map(|(_, n)| n).sum();
        let pace = self.studio().prompter.clamped().pace as f32;
        let now = Instant::now();
        self.rec.prompt.read += now.duration_since(last).as_secs_f32() * pace / 60.0;
        if self.rec.prompt.read >= total as f32 {
            self.rec.prompt.read = total as f32;
            self.rec.prompt.rolling = None;
            return false;
        }
        self.rec.prompt.rolling = Some(now);
        true
    }

    /// Moves the script by hand: `by` points up the page.
    fn scroll_prompter(&mut self, by: f32, cx: &mut Context<Self>) {
        let parts = self.script_parts();
        let counts: Vec<usize> = parts.iter().map(|(_, n)| *n).collect();
        let total: usize = counts.iter().sum();
        let Some((beat, n)) = parts.get(script::place(&counts, self.rec.prompt.read).0) else {
            return;
        };
        // How tall a word is here: its part's height shared among its words.
        let height = self
            .rec
            .prompt
            .parts
            .borrow()
            .get(beat)
            .map(|b| f32::from(b.size.height));
        let per_word = height.map_or(24.0, |h| (h / *n as f32).max(1.0));
        self.rec.prompt.read = (self.rec.prompt.read + by / per_word).clamp(0.0, total as f32);
        cx.notify();
    }

    /// The script over the scene: big, rolling past a reading line near the
    /// top (where a camera usually is).
    pub(super) fn render_prompter(
        &mut self,
        studio: &Studio,
        area: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !studio.prompter.on {
            return None;
        }
        let prompter = studio.prompter.clamped();
        let size = prompter.size;
        let (aw, ah) = (f32::from(area.size.width), f32::from(area.size.height));
        let beats: Vec<(usize, Beat)> = self
            .doc
            .project
            .story
            .beats
            .iter()
            .enumerate()
            .filter(|(_, b)| script::words(&b.script) > 0)
            .map(|(n, b)| (n, b.clone()))
            .collect();
        let shell = div()
            .id("prompter")
            .absolute()
            .inset_0()
            .overflow_hidden()
            .bg(gpui::rgba(0x000000c2));
        if beats.is_empty() {
            return Some(
                shell
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .p(px(24.0))
                    .child(
                        div()
                            .max_w(px(420.0))
                            .text_center()
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .text_color(Theme::subtext())
                            .child(
                                "No script yet. Load a text file, or paste one: a line starting with # begins \
                                 each part (a beat in the story), and anything in [square brackets] is a \
                                 direction for the cut, not said.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(
                                button("prompter-load", "Load script…")
                                    .on_click(cx.listener(|this, _, _, cx| this.load_script(cx))),
                            )
                            .child(
                                button("prompter-paste", "Paste")
                                    .on_click(cx.listener(|this, _, _, cx| this.paste_script(cx))),
                            ),
                    )
                    .into_any_element(),
            );
        }
        let counts: Vec<usize> = beats.iter().map(|(_, b)| script::words(&b.script)).collect();
        let total: usize = counts.iter().sum();
        self.rec.prompt.read = self.rec.prompt.read.clamp(0.0, total as f32);
        let read = self.rec.prompt.read;
        let (now, through) = script::place(&counts, read);
        // How far down the script the reading line is.
        let content = self.rec.prompt.content.get();
        let offset = self
            .rec
            .prompt
            .parts
            .borrow()
            .get(&beats[now].1.id)
            .map_or(0.0, |part| {
                f32::from(part.origin.y - content.origin.y) + through * f32::from(part.size.height)
            });
        let line = (ah * LINE).round();
        // Each part starts with its title: the line being read is under it.
        let title = (size * 0.34).max(11.0);
        let head = title * 1.3 + size * 0.2;
        let width = (aw - 96.0).min(size * 26.0).max(160.0);
        let left = ((aw - width) / 2.0).round();
        let parts: Vec<AnyElement> = beats
            .iter()
            .enumerate()
            .map(|(i, (n, beat))| {
                let id = beat.id;
                let cell = self.rec.prompt.parts.clone();
                div()
                    .relative()
                    .pb(px(size * 0.9))
                    .flex()
                    .flex_col()
                    .gap(px(size * 0.2))
                    .when(i != now, |d| d.opacity(0.45))
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                cell.borrow_mut().insert(id, bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .text_size(px(title))
                            .line_height(px(title * 1.3))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(beat.color))
                            .child(format!("{}  {}", n + 1, beat.title.to_uppercase())),
                    )
                    .child(
                        div()
                            .text_size(px(size))
                            .line_height(px(size * 1.4))
                            .text_color(gpui::white())
                            .child(beat.script.trim().to_string()),
                    )
                    .into_any_element()
            })
            .collect();
        let cell = self.rec.prompt.content.clone();
        let rolling = self.rec.prompt.rolling.is_some();
        let left_to_read = (total as f32 - read) * 60.0 / prompter.pace as f32;
        let status = if rolling {
            format!("Rolling · {} left · space pauses", clock(left_to_read))
        } else if read >= total as f32 - 0.5 {
            "The end · space starts again from the top".to_string()
        } else if self.rec.recording() {
            "Paused · space rolls".to_string()
        } else {
            format!(
                "{} to read · rolls when you record · space to rehearse · scroll to move",
                clock(left_to_read)
            )
        };
        Some(
            shell
                .cursor_pointer()
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    let delta = event.delta.pixel_delta(px(16.0));
                    this.scroll_prompter(-f32::from(delta.y), cx);
                    cx.stop_propagation();
                }))
                .on_click(cx.listener(|this, _, _, cx| this.roll_prompter(None, cx)))
                // The line being read: a band across, and a mark beside the words.
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(line))
                        .h(px(size * 1.4))
                        .bg(gpui::rgba(0xffffff0f)),
                )
                .child(
                    div()
                        .absolute()
                        .left(px((left - 18.0).max(4.0)))
                        .top(px(line))
                        .w(px(4.0))
                        .h(px(size * 1.4))
                        .rounded(px(2.0))
                        .bg(Theme::accent()),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(left))
                        .top(px(line - head - offset))
                        .w(px(width))
                        .flex()
                        .flex_col()
                        .child(
                            canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                                .absolute()
                                .size_full(),
                        )
                        .children(parts)
                        .child(
                            div()
                                .text_size(px(title))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(Theme::overlay())
                                .child("END"),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(10.0))
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .h(px(22.0))
                                .px(px(10.0))
                                .rounded(px(11.0))
                                .bg(gpui::rgba(0x000000cc))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .when(rolling, |d| {
                                    d.child(div().size(px(6.0)).rounded_full().bg(Theme::accent()))
                                })
                                .child(div().text_size(px(10.5)).text_color(Theme::subtext()).child(status)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The Record tab's teleprompter controls.
    pub(super) fn prompter_panel(&mut self, studio: &Studio, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let prompter = studio.prompter.clamped();
        let parts = self.script_parts();
        let words: usize = parts.iter().map(|(_, n)| n).sum();
        let rolling = self.rec.prompt.rolling.is_some();
        let summary = if parts.is_empty() {
            "No script yet: load a text file or paste one. It becomes beats in the story.".to_string()
        } else {
            format!(
                "{} beat{} · {words} words · about {} at this pace",
                parts.len(),
                if parts.len() == 1 { "" } else { "s" },
                clock(words as f32 * 60.0 / prompter.pace as f32)
            )
        };
        let label = |text: &'static str| {
            div()
                .w(px(64.0))
                .text_size(px(11.5))
                .text_color(Theme::overlay())
                .child(text)
        };
        let value = |text: String| {
            div()
                .w(px(68.0))
                .text_center()
                .font_family(MONO_FONT)
                .text_size(px(11.0))
                .text_color(Theme::text())
                .child(text)
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .id("prompter-switch")
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .child(caps("Teleprompter"))
                    .child(super::record::switch(prompter.on))
                    .tooltip(tip(
                        "Show the script over the scene to read while you record (it is not in the recording)",
                        None,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.set_prompter(cx, |p| p.on = !p.on))),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(Theme::overlay())
                    .child(summary),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.0))
                    .child(
                        button("script-load", "Load script…")
                            .tooltip(tip(
                                "A text file: a line starting with # begins each beat; [square brackets] are \
                                 directions for the cut",
                                None,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.load_script(cx))),
                    )
                    .child(
                        button("script-paste", "Paste")
                            .tooltip(tip("Use the text on the clipboard as the script", None))
                            .on_click(cx.listener(|this, _, _, cx| this.paste_script(cx))),
                    )
                    .when(!parts.is_empty(), |d| {
                        d.child(
                            button("script-edit", "Edit in Story")
                                .tooltip(tip("Each beat's lines and directions are on its card", Some("⌘3")))
                                .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Story, cx))),
                        )
                    }),
            )
            .when(prompter.on, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(label("Pace"))
                        .child(nudge("pace-less", "minus").on_click(
                            cx.listener(|this, _, _, cx| this.set_prompter(cx, |p| p.pace = p.pace.saturating_sub(10))),
                        ))
                        .child(value(format!("{} wpm", prompter.pace)))
                        .child(
                            nudge("pace-more", "plus")
                                .on_click(cx.listener(|this, _, _, cx| this.set_prompter(cx, |p| p.pace += 10))),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(label("Text"))
                        .child(
                            nudge("text-less", "minus")
                                .on_click(cx.listener(|this, _, _, cx| this.set_prompter(cx, |p| p.size -= 4.0))),
                        )
                        .child(value(format!("{:.0} pt", prompter.size)))
                        .child(
                            nudge("text-more", "plus")
                                .on_click(cx.listener(|this, _, _, cx| this.set_prompter(cx, |p| p.size += 4.0))),
                        ),
                )
                .when(!parts.is_empty(), |d| {
                    d.child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(
                                button("prompter-roll", if rolling { "Pause" } else { "Roll" })
                                    .tooltip(tip(
                                        "Roll the script without recording, to rehearse. Recording rolls it too.",
                                        Some("Space"),
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| this.roll_prompter(None, cx))),
                            )
                            .child(
                                button("prompter-top", "From the top").on_click(cx.listener(|this, _, _, cx| {
                                    this.rec.prompt.read = 0.0;
                                    cx.notify();
                                })),
                            ),
                    )
                })
            })
    }

    // Cutting a reading to the script.

    /// Cuts the selected take to the script.
    pub(super) fn cut_to_script_action(&mut self, cx: &mut Context<Self>) {
        let clips = self.selection.clone();
        if clips.is_empty() {
            self.say_error("Select the take you read the script in, then cut it to the script");
        } else if let Err(e) = self.read_take(clips, true, None, cx) {
            self.say_error(e);
        }
        cx.notify();
    }

    /// Hears what is said in a take and finds where each beat's lines were
    /// read (the last time, when a line was said again). With `cut`, the
    /// take is cut to the story: each beat gets its reading, on every layer
    /// of the take, and becomes as long. `clips` name the take: its layers,
    /// or just one of them (the rest are those recorded alongside it).
    pub(super) fn read_take(
        &mut self,
        clips: Vec<ClipId>,
        cut: bool,
        reply: Option<Sender<String>>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let project = &self.doc.project;
        if !project.has_script() {
            return Err(
                "no beat has lines to say: load a script (`splice ctl story script --file F`), or give beats \
                 theirs with `story set --beat B --script TEXT`"
                    .into(),
            );
        }
        let named: Vec<Clip> = clips.iter().filter_map(|id| project.clip(*id)).cloned().collect();
        if named.is_empty() || named.len() != clips.len() {
            return Err("name the take with --clip C: the clip with the voice in it".into());
        }
        let sounds =
            |c: &Clip| c.generator.is_none() && !c.is_retimed() && project.media(c.media).is_some_and(|m| m.has_audio);
        // The microphone's own file before a camera's sound.
        let voice = named
            .iter()
            .filter(|c| sounds(c))
            .min_by_key(|c| project.media(c.media).is_some_and(|m| m.kind != MediaKind::Audio))
            .ok_or("none of those clips has a voice to hear (a retimed clip cannot be cut to the script)")?
            .clone();
        let layers: Vec<ClipId> = if named.len() > 1 {
            clips
        } else {
            let frame = project.frame();
            project
                .clips()
                .map(|(_, c)| c)
                .filter(|c| {
                    c.generator.is_none()
                        && (c.start - voice.start).abs() <= frame
                        && (c.duration - voice.duration).abs() <= SECOND / 2
                })
                .map(|c| c.id)
                .collect()
        };
        if crate::speech::whisper().is_none() {
            return Err(crate::speech::INSTALL.replace("Captions need", "Hearing a take needs"));
        }
        let media = project.media(voice.media).ok_or("that clip has no media")?;
        let path = self
            .library
            .path(&media.hash)
            .ok_or("its media is not ready yet")?
            .to_path_buf();
        if !self.captioning.insert(voice.id) {
            return Err("that clip is being heard already".into());
        }
        self.say(if crate::speech::has_model() {
            "Listening for the script…".to_string()
        } else {
            format!(
                "Downloading the speech model ({} MB, once), then listening…",
                crate::speech::MODEL_SIZE_MB
            )
        });
        cx.notify();
        let (id, from, span) = (voice.id, voice.source_in, voice.span());
        cx.spawn(async move |this, cx| {
            let heard = cx
                .background_spawn(async move { crate::speech::transcribe(&path, from, span) })
                .await;
            this.update(cx, |editor, cx| {
                editor.captioning.remove(&id);
                let answer = match heard {
                    Ok(heard) => editor.match_reading(id, from, heard, &layers, cut, cx),
                    Err(e) => Err(e.to_string()),
                };
                let answer = answer.unwrap_or_else(|e| {
                    editor.say_error(format!("Script: {e}"));
                    crate::ctl::fail(e)
                });
                if let Some(reply) = reply {
                    let _ = reply.send(answer.to_string());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        Ok(())
    }

    /// What was heard in the voice clip, matched to the script; cut to it
    /// when asked.
    fn match_reading(
        &mut self,
        voice: ClipId,
        from: Time,
        heard: Vec<crate::speech::Heard>,
        layers: &[ClipId],
        cut: bool,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        // Where the clip is now: it may have moved while it was heard.
        let clip = self
            .doc
            .project
            .clip(voice)
            .filter(|c| c.source_in == from && !c.is_retimed())
            .ok_or("the clip was changed while it was heard; try again")?;
        let (start, end) = (clip.start, clip.end());
        if heard.is_empty() {
            return Err("no speech heard in that clip".into());
        }
        let words: Vec<splice_core::captions::Word> = heard
            .into_iter()
            .map(|w| splice_core::captions::Word {
                text: w.text,
                start: start + w.start,
                end: start + w.end,
            })
            .collect();
        let beats: Vec<Beat> = self
            .doc
            .project
            .story
            .beats
            .iter()
            .filter(|b| script::words(&b.script) > 0)
            .cloned()
            .collect();
        let scripts: Vec<&str> = beats.iter().map(|b| b.script.as_str()).collect();
        // The air left around a reading stays inside the take.
        let found: Vec<Option<script::Reading>> = script::align(&scripts, &words)
            .into_iter()
            .map(|r| {
                r.map(|r| script::Reading {
                    start: r.start.max(start),
                    end: r.end.min(end),
                    ..r
                })
            })
            .collect();
        let readings: Vec<Value> = beats
            .iter()
            .zip(&found)
            .map(|(beat, reading)| match reading {
                Some(r) => json!({
                    "beat": beat.id.0, "title": beat.title, "read": true,
                    "start": secs(r.start), "end": secs(r.end), "seconds": secs(r.end - r.start),
                    "score": (r.score * 100.0).round() / 100.0,
                    "script": beat.script, "heard": r.heard,
                }),
                None => json!({ "beat": beat.id.0, "title": beat.title, "read": false, "script": beat.script }),
            })
            .collect();
        let spans: Vec<(BeatId, Time, Time)> = beats
            .iter()
            .zip(&found)
            .filter_map(|(b, r)| r.as_ref().map(|r| (b.id, r.start, r.end)))
            .collect();
        let unread: Vec<&str> = beats
            .iter()
            .zip(&found)
            .filter(|(_, r)| r.is_none())
            .map(|(b, _)| b.title.as_str())
            .collect();
        let said = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        if !cut {
            self.say(format!("Heard {} of {} beats in the take", spans.len(), beats.len()));
            return Ok(json!({
                "ok": true, "clip": voice.0, "layers": layers.iter().map(|c| c.0).collect::<Vec<_>>(),
                "readings": readings, "heard": said,
                "note": "start and end are timeline seconds now; `story cut` lays each reading into its beat",
            }));
        }
        if spans.is_empty() {
            return Err("none of the script was heard in that clip".into());
        }
        self.typing = None;
        let pieces = self.edit(cx, |p| p.lay_reading(layers, &spans));
        if pieces.is_empty() {
            return Err("the take could not be cut: its clips were changed or retimed".into());
        }
        self.selection = pieces.clone();
        self.say(if unread.is_empty() {
            format!("Cut to the script: {} beats, each with its reading", spans.len())
        } else {
            format!(
                "Cut to the script: {} of {} beats; not heard: {}",
                spans.len(),
                beats.len(),
                unread.join(", ")
            )
        });
        let mut out = self.edited_with(json!({
            "clips": pieces.iter().map(|c| c.0).collect::<Vec<_>>(),
            "readings": readings,
            "heard": said,
            "note": "readings' start and end are where each was in the take before the cut; `story read` \
                     shows where the beats and their clips are now",
        }));
        out["story"] = self.story_json();
        Ok(out)
    }

    // splice ctl.

    /// `splice ctl story match|cut --clip C`: answered once the take is heard.
    pub(super) fn ctl_reading(
        &mut self,
        request: &Value,
        reply: Sender<String>,
        cx: &mut Context<Self>,
    ) -> Option<Value> {
        use crate::ctl::fail;
        let clips = match self.clips_of(request) {
            Ok(ids) if !ids.is_empty() => ids,
            Ok(_) => return Some(fail("name the take with --clip C: the clip with the voice in it")),
            Err(e) => return Some(fail(e)),
        };
        let cut = request["sub"] == json!("cut");
        match self.read_take(clips, cut, Some(reply), cx) {
            Ok(()) => None,
            Err(e) => Some(fail(e)),
        }
    }

    /// `splice ctl story script`: the script, or a new one (`--file`, `--text`).
    pub(super) fn ctl_script(&mut self, request: &Value, cx: &mut Context<Self>) -> Value {
        let text = request
            .get("content")
            .or_else(|| request.get("text"))
            .and_then(Value::as_str);
        let Some(text) = text else {
            let project = &self.doc.project;
            let pace = self.studio().prompter.clamped().pace;
            let words: usize = self.script_parts().iter().map(|(_, n)| n).sum();
            return json!({
                "ok": true,
                "text": project.script_text(),
                "words": words,
                "pace": pace,
                "seconds": words as f64 * 60.0 / pace as f64,
                "note": "each beat's lines and directions are in `story read` as script and directions",
            });
        };
        let replace = request.get("replace").and_then(Value::as_bool) == Some(true);
        match self.add_script_text(text, replace, cx) {
            Ok(ids) => {
                let mut out = self.edited_with(json!({ "beats": ids.iter().map(|b| b.0).collect::<Vec<_>>() }));
                out["story"] = self.story_json();
                out
            }
            Err(e) => crate::ctl::fail(e),
        }
    }

    /// `splice ctl story direction add|set|remove`.
    pub(super) fn ctl_direction(&mut self, request: &Value, cx: &mut Context<Self>) -> Value {
        use crate::ctl::fail;
        let note = request.get("note").and_then(Value::as_str).map(str::to_string);
        let done = request.get("done").and_then(Value::as_bool);
        let named = request.get("direction").and_then(Value::as_u64).map(DirectionId);
        let action = request.get("action").and_then(Value::as_str).unwrap_or("add");
        self.typing = None;
        let direction = match action {
            "add" => {
                let Some(beat) = request.get("beat").and_then(Value::as_u64).map(BeatId) else {
                    return fail("name the beat with --beat B");
                };
                let Some(note) = note.filter(|n| !n.trim().is_empty()) else {
                    return fail("say what to do with --note TEXT");
                };
                match self.edit(cx, |p| p.add_direction(beat, &note)) {
                    Some(id) => id,
                    None => return fail(format!("no beat {}; `splice ctl story read` lists them", beat.0)),
                }
            }
            "set" | "remove" => {
                let Some(id) = named.filter(|d| self.doc.project.direction_beat(*d).is_some()) else {
                    return fail("name the direction with --direction D; `splice ctl story read` lists them");
                };
                if action == "remove" {
                    self.edit(cx, |p| p.remove_direction(id));
                } else {
                    self.edit(cx, |p| {
                        p.set_direction(id, |d| {
                            if let Some(n) = note {
                                d.note = n.trim().to_string();
                            }
                            if let Some(done) = done {
                                d.done = done;
                            }
                        })
                    });
                }
                id
            }
            other => return fail(format!("unknown `story direction {other}`: use add, set or remove")),
        };
        let mut out = self.edited_with(json!({ "direction": direction.0 }));
        out["story"] = self.story_json();
        out
    }

    /// The teleprompter as `record status` and `record prompter` show it.
    pub(super) fn prompter_json(&self) -> Value {
        let prompter = self.studio().prompter.clamped();
        let words: usize = self.script_parts().iter().map(|(_, n)| n).sum();
        let read = self.rec.prompt.read.min(words as f32);
        json!({
            "on": prompter.on,
            "pace": prompter.pace,
            "size": prompter.size,
            "showing": self.prompting(),
            "rolling": self.rec.prompt.rolling.is_some(),
            "words": words,
            "read": read.floor(),
            "seconds_left": (words as f32 - read) * 60.0 / prompter.pace as f32,
        })
    }

    /// `splice ctl record prompter [on|off|start|pause|top] [--pace N] [--size PT]`.
    pub(super) fn ctl_prompter(&mut self, request: &Value, cx: &mut Context<Self>) -> Value {
        use crate::ctl::fail;
        let action = request.get("action").and_then(Value::as_str);
        let pace = request.get("pace").and_then(Value::as_u64);
        let size = request.get("size").and_then(Value::as_f64);
        let on = match action {
            Some("on") => Some(true),
            Some("off") => Some(false),
            None | Some("start" | "pause" | "top") => None,
            Some(other) => {
                return fail(format!(
                    "unknown `record prompter {other}`: use on, off, start, pause or top"
                ));
            }
        };
        if on.is_some() || pace.is_some() || size.is_some() {
            self.set_prompter(cx, |p| {
                p.on = on.unwrap_or(p.on);
                p.pace = pace.map_or(p.pace, |n| n.min(10_000) as u32);
                p.size = size.map_or(p.size, |s| s as f32);
            });
        }
        match action {
            Some("start") if !self.prompting() => {
                return fail(
                    "the teleprompter is not showing: it needs a script (`story script --file F`), to be on \
                     (`record prompter on`) and the Record tab (`view record`)",
                );
            }
            Some("start") => self.roll_prompter(Some(true), cx),
            Some("pause") => self.roll_prompter(Some(false), cx),
            Some("top") => self.rec.prompt.read = 0.0,
            _ => {}
        }
        cx.notify();
        json!({ "ok": true, "prompter": self.prompter_json() })
    }
}
