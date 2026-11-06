//! Add project, after t3code's: a palette with a path field that starts at `~/` and the folders of
//! what is typed under it, as on a shell's tab completion. A project is a folder; adding one starts
//! a Claude session there, and the sidebar groups sessions by their folder.
//!
//! Keys: typing filters the folders by what follows the last `/`; Up/Down pick one; Enter goes into
//! the picked folder, or adds the typed path when none is picked; mod+Enter adds the picked folder;
//! Tab completes; Escape, or Backspace in an empty field, closes. A path that does not exist yet is
//! made ("Create & Add").

use femme_proto::rpc::{Req, Resp};
use gpui::prelude::FluentBuilder as _;
use gpui::AppContext as _;
use gpui::{
    div, px, relative, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::Sizable as _;

use crate::motion;
use crate::net::Net;
use crate::session;
use crate::theme::{self, bad, ink, paper, BODY_SIZE, FONT_MONO, FONT_UI};
use crate::ui::{button, dialog, meta, row, ruled, title_bar};

/// Where the field starts.
const START: &str = "~/";

/// The palette closed (the app gives the keyboard back).
pub struct Closed;

/// The folder to list for `text` and the start of a name to filter its folders by: `~/fe` is
/// `~/` and `fe`, `~` is `~` and nothing.
pub fn split(text: &str) -> (&str, &str) {
    match text.rfind('/') {
        Some(i) => (&text[..=i], &text[i + 1..]),
        None if text == "~" => (text, ""),
        None => ("", text),
    }
}

/// The folders of a listing that start with `filter`, case aside. Hidden ones only once the filter
/// starts with a dot.
pub fn matching<'a>(names: &'a [String], filter: &str) -> Vec<&'a str> {
    let f = filter.to_lowercase();
    names
        .iter()
        .map(String::as_str)
        .filter(|n| f.starts_with('.') || !n.starts_with('.'))
        .filter(|n| n.to_lowercase().starts_with(&f))
        .collect()
}

/// The field after going into `name` of folder `dir`; `..` goes up one.
pub fn into(dir: &str, name: &str) -> String {
    let dir = if dir == "~" { "~/" } else { dir };
    if name != ".." {
        return format!("{dir}{name}/");
    }
    let trimmed = dir.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[..=i].to_string(),
        // `~/` up is the folder above home: the daemon refuses that, so it stays.
        None => dir.to_string(),
    }
}

/// What Tab completes `filter` to: the longest start the matches share.
pub fn common<'a>(names: &[&'a str]) -> Option<&'a str> {
    let first = *names.first()?;
    let len = names.iter().fold(first.len(), |len, n| {
        first
            .char_indices()
            .zip(n.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, a), _)| (i + a.len_utf8()).min(len))
    });
    Some(&first[..len])
}

/// What the daemon said about the folder being listed.
#[derive(Clone, Debug, PartialEq)]
enum Listing {
    Loading,
    Folders(Vec<String>),
    /// No such folder: Enter makes it.
    Missing,
    Failed(String),
}

/// The add project palette: closed until [`Picker::open`]; mount it over the centre.
pub struct Picker {
    net: Option<Entity<Net>>,
    open: bool,
    input: Option<Entity<InputState>>,
    /// The folder last asked for, and its answer.
    dir: String,
    listing: Listing,
    /// The picked folder, by its place among the matches.
    pick: Option<usize>,
    /// An add is out.
    busy: bool,
    error: Option<String>,
    focus: FocusHandle,
    /// The keyboard goes to the field at the next render.
    focus_field: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<Closed> for Picker {}

impl Picker {
    /// A closed palette that asks `net` for folders and sessions.
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        Self {
            net,
            open: false,
            input: None,
            dir: String::new(),
            listing: Listing::Loading,
            pick: None,
            busy: false,
            error: None,
            focus: cx.focus_handle(),
            focus_field: false,
            _subs: Vec::new(),
        }
    }

    /// Whether the palette is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the palette at `~/`.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        self.focus_field = true;
        self.error = None;
        self.pick = None;
        self.dir.clear();
        cx.notify();
    }

    /// Closes the palette; [`Closed`] is announced.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.busy = false;
        cx.emit(Closed);
        cx.notify();
    }

    fn text(&self, cx: &App) -> String {
        self.input
            .as_ref()
            .map_or_else(String::new, |i| i.read(cx).value().to_string())
    }

    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = &self.input {
            i.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        self.pick = None;
        self.changed(cx);
    }

    /// The field changed: list its folder when that is another one.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let text = self.text(cx);
        let dir = split(&text).0.to_string();
        if dir != self.dir || self.listing == Listing::Loading {
            self.dir = dir.clone();
            self.listing = Listing::Loading;
            self.pick = None;
            self.list(dir, cx);
        }
        cx.notify();
    }

    fn list(&mut self, dir: String, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let req = Req::Folders {
            path: dir.clone(),
            make: false,
        };
        let sent = net.update(cx, |n, cx| n.try_run(req, cx));
        cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                if t.dir != dir {
                    return;
                }
                t.listing = match answer {
                    Ok(Resp::Folders { names, .. }) => Listing::Folders(names),
                    Err(e) if e.contains("NotFound") => Listing::Missing,
                    Err(e) => Listing::Failed(e),
                    Ok(other) => Listing::Failed(format!("unexpected answer {other:?}")),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The folders shown under the field: `..` first (away from home), then the matches.
    fn rows(&self, cx: &App) -> Vec<String> {
        let text = self.text(cx);
        let (dir, filter) = split(&text);
        let mut rows = Vec::new();
        if !matches!(dir, "" | "~" | "~/") {
            rows.push("..".to_string());
        }
        if let Listing::Folders(names) = &self.listing {
            rows.extend(matching(names, filter).into_iter().map(str::to_owned));
        }
        rows
    }

    /// The typed path names a folder that is not there: adding makes it.
    fn creates(&self, cx: &App) -> bool {
        let text = self.text(cx);
        let (_, filter) = split(&text);
        match &self.listing {
            Listing::Missing => true,
            Listing::Folders(names) => !filter.is_empty() && !names.iter().any(|n| n == filter),
            _ => false,
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let rows = self.rows(cx);
        let text = self.text(cx);
        let (dir, filter) = split(&text);
        let (dir, filter) = (dir.to_string(), filter.to_string());
        match k.key.as_str() {
            "escape" => self.close(cx),
            "backspace" if text.is_empty() => self.close(cx),
            "down" if !rows.is_empty() => {
                self.pick = Some(self.pick.map_or(0, |p| (p + 1).min(rows.len() - 1)));
                cx.notify();
            }
            "up" if !rows.is_empty() => {
                self.pick = self.pick.and_then(|p| p.checked_sub(1));
                cx.notify();
            }
            "tab" => {
                let names: Vec<&str> = rows.iter().map(String::as_str).filter(|r| *r != "..").collect();
                if let [only] = names.as_slice() {
                    self.set_text(into(&dir, only), window, cx);
                } else if let Some(c) = common(&names).filter(|c| c.len() > filter.len()) {
                    self.set_text(format!("{dir}{c}"), window, cx);
                }
            }
            "enter" => match self.pick.and_then(|p| rows.get(p)) {
                Some(name) if (k.modifiers.platform || k.modifiers.control) => {
                    let path = into(&dir, name);
                    self.add(path, cx);
                }
                Some(name) => self.set_text(into(&dir, name), window, cx),
                None => self.add(text, cx),
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Makes the folder if it is missing, then starts a Claude session in it.
    fn add(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let made = net.update(cx, |n, cx| n.try_run(Req::Folders { path, make: true }, cx));
            let cwd = match made.await {
                Ok(Resp::Folders { path, .. }) => Ok(path),
                Ok(other) => Err(format!("unexpected answer {other:?}")),
                Err(e) => Err(e),
            };
            let created = match cwd {
                Ok(cwd) => {
                    let req = session::new_claude(&cwd);
                    net.update(cx, |n, cx| n.try_run(req, cx)).await
                }
                Err(e) => Err(e),
            };
            this.update(cx, |t, cx| {
                t.busy = false;
                match created {
                    Ok(Resp::Session(sid)) => {
                        net.update(cx, |n, cx| n.set_current(Some(sid), cx));
                        t.close(cx);
                    }
                    Ok(other) => t.error = Some(format!("unexpected answer {other:?}")),
                    Err(e) => t.error = Some(format!("Failed to add project: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder("Enter path (e.g. ~/projects/my-app)")
            });
            self._subs = vec![cx.subscribe_in(&input, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    t.pick = None;
                    t.changed(cx);
                }
            })];
            self.input = Some(input);
        }
        if std::mem::take(&mut self.focus_field) {
            cx.defer_in(window, |t, window, cx| {
                t.set_text(START.into(), window, cx);
                if let Some(i) = &t.input {
                    i.update(cx, |s, cx| s.focus(window, cx));
                }
            });
        }
    }

    fn folder_row(&self, ix: usize, name: &str, dir: &str, cx: &mut Context<Self>) -> AnyElement {
        let path = into(dir, name);
        row(SharedString::from(format!("pp-{ix}")), self.pick == Some(ix))
            .min_h(px(26.))
            .font_family(FONT_MONO)
            .on_click(cx.listener(move |t, _, window, cx| t.set_text(path.clone(), window, cx)))
            .child(div().truncate().child(if name == ".." {
                "..".to_string()
            } else {
                format!("{name}/")
            }))
            .into_any_element()
    }

    fn card(&self, cx: &mut Context<Self>) -> AnyElement {
        let text = self.text(cx);
        let dir = split(&text).0.to_string();
        let rows = self.rows(cx);
        let creates = self.creates(cx);
        let mod_key = if cfg!(target_os = "macos") { "⌘" } else { "ctrl" };
        let hint = if self.pick.is_some() {
            format!("{mod_key} enter")
        } else {
            "enter".into()
        };
        let field = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .p(px(6.))
            .child(
                ruled(div().flex_1().min_w_0().h(px(28.)).flex().items_center().bg(paper()))
                    .child(Input::new(self.input.as_ref().unwrap()).appearance(false).small()),
            )
            .child(
                button("pp-add", if creates { "Create & Add" } else { "Add" })
                    .when(self.busy, |b| b.opacity(0.5))
                    .on_click(cx.listener(|t, _, _, cx| {
                        let text = t.text(cx);
                        t.add(text, cx);
                    })),
            )
            .child(meta(hint));
        let empty = match &self.listing {
            Listing::Loading => Some("Loading folders...".to_string()),
            Listing::Missing => {
                Some("Press Enter to create this folder and add it as a project.".into())
            }
            Listing::Failed(e) => Some(e.clone()),
            Listing::Folders(_) if creates && rows.iter().all(|r| r == "..") => {
                Some("Press Enter to create this folder and add it as a project.".into())
            }
            Listing::Folders(_) => None,
        };
        let list = div()
            .id("pp-list")
            .max_h(px(320.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(ix, name)| self.folder_row(ix, name, &dir, cx)),
            );
        dialog()
            .w(px(560.))
            .max_w(relative(0.95))
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(title_bar("add project"))
            .child(field)
            .child(
                div()
                    .px(px(10.))
                    .pb(px(2.))
                    .child(meta("Directories")),
            )
            .child(list)
            .when_some(empty, |d, e| {
                d.child(div().px(px(10.)).py(px(6.)).child(meta(e)))
            })
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .child(meta(e).text_color(bad())),
                )
            })
            .into_any_element()
    }
}

impl Focusable for Picker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The layer fades and the card rises 6 px into place; both go the same way, drawn until gone.
        let Some(a) = motion::appear("picker-layer", self.open, window, cx) else {
            return div().into_any_element();
        };
        if self.open {
            self.sync(window, cx);
        }
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(relative(0.12))
            .bg(theme::scrim())
            .opacity(a.opacity())
            .occlude()
            .key_context("Picker")
            .track_focus(&self.focus)
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.4))
            .text_color(ink())
            // Before the field: Enter, Tab and the arrows are the palette's, not the field's.
            .capture_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(|t, _, _, cx| t.close(cx)))
            .child(div().mt(a.offset(6.)).child(self.card(cx)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_is_a_folder_and_the_start_of_a_name() {
        assert_eq!(split("~/"), ("~/", ""));
        assert_eq!(split("~/fe"), ("~/", "fe"));
        assert_eq!(split("~/femme-work/F"), ("~/femme-work/", "F"));
        assert_eq!(split("~"), ("~", ""));
        assert_eq!(split("src"), ("", "src"));
        assert_eq!(split("/home/u/"), ("/home/u/", ""));
    }

    #[test]
    fn folders_match_by_their_start_and_hidden_ones_on_a_dot() {
        let names: Vec<String> = [".cache", "Documents", "femme", "femme-work"]
            .map(String::from)
            .to_vec();
        assert_eq!(matching(&names, ""), ["Documents", "femme", "femme-work"]);
        assert_eq!(matching(&names, "FE"), ["femme", "femme-work"]);
        assert_eq!(matching(&names, "."), [".cache"]);
    }

    #[test]
    fn going_into_a_folder_and_back_up() {
        assert_eq!(into("~/", "femme"), "~/femme/");
        assert_eq!(into("~", "femme"), "~/femme/");
        assert_eq!(into("~/femme/", ".."), "~/");
        assert_eq!(into("~/a/b/", ".."), "~/a/");
        assert_eq!(into("~/", ".."), "~/");
        assert_eq!(into("/home/u/x/", ".."), "/home/u/");
    }

    #[test]
    fn tab_completes_what_the_matches_share() {
        assert_eq!(common(&["femme", "femme-work"]), Some("femme"));
        assert_eq!(common(&["abc", "abd"]), Some("ab"));
        assert_eq!(common(&["x", "y"]), Some(""));
        assert_eq!(common(&[]), None);
    }
}
