// SPDX-License-Identifier: GPL-3.0-or-later
//! Bases: database views of your notes, from Obsidian's `.base` files.
//!
//! A Base picks notes by what is in them (their properties, tags, links,
//! folder, dates) and shows them as a table, a list or cards, sorted,
//! grouped and with computed columns. Open a `.base` file and its first
//! view comes up; the file itself is YAML, one key away (`e`).
//!
//! Table and list views are text: `RET` visits the note on a line, and
//! everything you know works (search, copy, narrow). Cards, kanban and map
//! views are drawn on Forge's canvas: click a card or pin to open the note.
//! On a kanban board drag a card to another column to set the grouped
//! property (`groupBy`, with `columns: [...]` for the order). On a map drag
//! to pan, scroll to zoom, `+` `-` `0` zoom and fit.
//!
//! Keys in a view: `RET` open, `v` / `]` / `[` change view, `f` filter by
//! an expression (`F` clears it), `s` sort by a property (again to flip),
//! `g` refresh, `e` the .base file, `c` copy as CSV, `q` leave.
//!
//! Settings (init.rhai):
//!
//! ```rhai
//! forge.plugin.config("bases", #{ vault: "~/Notes", column_width: 32 });
//! forge.plugin.grant("bases", #{ directories: ["~/Notes"] });
//! ```
//!
//!   cargo build --release --target wasm32-wasip2
//!   cp target/wasm32-wasip2/release/forge_bases.wasm ~/.config/forge/plugins/bases.wasm

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use forge_notes::base::{self, Base, Kind, Rendered, SortKey, Table};
use forge_notes::geo::{self, MAP_H, MAP_W, MapOpts, MapView, Pin};
use forge_notes::jsoncanvas::Event as PointerEvent;
use forge_notes::vault::{Index, RawNote, is_note_file, set_property_in};
use forge_plugin_sdk::{
    Buffer, EventKind, MenuItem, Position, Span, bind_in_mode, canvas, command, completing_read,
    define_menu_for_mode, files, forge_plugin, menu_item, menu_separator, message, modes, on,
    read_string, run_command,
};
use serde_json::Value;

const MODE: &str = "base-mode";
const VIEW_MODE: &str = "base-view-mode";
const CARDS_MODE: &str = "base-cards-mode";

#[derive(Clone)]
struct Config {
    vault: Option<String>,
    column_width: usize,
    max_notes: usize,
    auto_view: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { vault: None, column_width: 32, max_notes: 5000, auto_view: true }
    }
}

fn parse_config(json: &str) -> Result<Config, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bases: settings: {e}"))?;
    let map = v.as_object().ok_or("bases: settings must be a table")?;
    let mut c = Config::default();
    for (k, v) in map {
        match k.as_str() {
            "vault" => c.vault = Some(v.as_str().ok_or("bases: vault must be text")?.to_string()),
            "column_width" => c.column_width = v.as_u64().ok_or("bases: column_width must be a number")?.clamp(8, 120) as usize,
            "max_notes" => c.max_notes = v.as_u64().ok_or("bases: max_notes must be a number")? as usize,
            "auto_view" => c.auto_view = v.as_bool().ok_or("bases: auto_view must be true or false")?,
            other => return Err(format!("bases: unknown setting {other} (vault, column_width, max_notes, auto_view)")),
        }
    }
    Ok(c)
}

/// One open view of a Base.
#[derive(Default)]
struct Open {
    /// The .base file's buffer.
    source: u32,
    root: String,
    view: usize,
    extra: Option<String>,
    sort: Option<SortKey>,
    rendered: Option<Rendered>,
    index: Option<Index>,
    /// Cards, kanban, map: the canvas's name; None for text views.
    canvas: Option<String>,
    /// Kanban: each lane's value, and the property they are values of.
    lanes: Vec<String>,
    lane_prop: Option<String>,
    /// Map: the pins, how the view reads them and where it is looking.
    pins: Vec<Pin>,
    map_opts: MapOpts,
    map: Option<MapView>,
    /// A press waiting for its release: (note, x, y) and whether it moved.
    press: Option<(Option<usize>, f32, f32)>,
    moved: bool,
    /// The note a right-click was on (for the menu's commands).
    ctx_note: Option<usize>,
    /// The page width a kanban board was laid out for.
    page_w: f32,
    /// The properties of the columns a text table shows, left to right.
    col_props: Vec<String>,
}

thread_local! {
    static CONFIG: RefCell<Config> = RefCell::new(Config::default());
    /// Open views by the name of their buffer.
    static OPEN: RefCell<HashMap<String, Open>> = RefCell::new(HashMap::new());
    static NEXT: RefCell<u32> = const { RefCell::new(0) };
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn whole() -> Span {
    Span { start: Position { line: 0, column: 0 }, end: Position { line: u32::MAX, column: u32::MAX } }
}

fn vault_for(path: &str) -> String {
    if let Some(v) = CONFIG.with(|c| c.borrow().vault.clone()) {
        return v.trim_end_matches('/').to_string();
    }
    // The nearest folder above the file holding `.obsidian`, else its own.
    match files::locate_dominating_file(path, ".obsidian") {
        Some(dir) => files::directory_name(&dir),
        None => files::directory_name(&files::name_directory(path).unwrap_or_default()),
    }
}

/// Read the vault's notes.
fn build_index(root: &str) -> Result<Index, String> {
    let max = CONFIG.with(|c| c.borrow().max_notes);
    let mut raws = Vec::new();
    for p in files::list_dir(root, true)? {
        if p.ends_with('/') || !is_note_file(&p) {
            continue;
        }
        if raws.len() >= max {
            break;
        }
        let Ok(text) = files::read(&p) else { continue };
        let info = files::stat(&p);
        raws.push(RawNote {
            path: forge_notes::vault::relative_to(root, &p),
            text,
            size: info.as_ref().map_or(0, |i| i.size),
            ctime: info.as_ref().map_or(0, |i| i.created),
            mtime: info.as_ref().map_or(0, |i| i.modified),
        });
    }
    Ok(Index::build(&raws))
}

fn buffer_name(source: &Buffer) -> String {
    let n = source.name();
    format!("*Base: {}*", n.strip_suffix(".base").unwrap_or(&n))
}

fn parse_source(source: &Buffer) -> Result<Base, String> {
    Base::parse(&source.text())
}

/// Draw (or redraw) the view whose buffer is named `name`.

/// Scenes name colour roles (`card`, `line` ...); Forge picks the colours
/// when it draws them, so a theme or light/dark change recolours every
/// picture at once. Nothing to fetch from the theme here.
fn apply_theme() {
    forge_canvas::Palette::set_current(forge_canvas::Palette::tokens());
}


fn render(name: &str, reindex: bool) -> Result<(), String> {
    apply_theme();
    let (source_id, root, view, extra, sort, need_index) = OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(name).ok_or("This isn't a Base view")?;
        Ok::<_, String>((v.source, v.root.clone(), v.view, v.extra.clone(), v.sort.clone(), reindex || v.index.is_none()))
    })?;
    let source = Buffer::from_id(source_id).filter(Buffer::alive).ok_or("The .base file's buffer is gone")?;
    let mut base = parse_source(&source)?;
    if let Some(s) = &sort {
        if let Some(v) = base.views.get_mut(view) {
            v.sort = vec![s.clone()];
        }
    }
    if need_index {
        let index = build_index(&root)?;
        OPEN.with(|o| o.borrow_mut().get_mut(name).map(|v| v.index = Some(index)));
    }
    let view_idx = view.min(base.views.len().saturating_sub(1));
    if base.views.is_empty() {
        return Err("This Base has no views".into());
    }
    let (table, index): (Table, Index) = OPEN.with(|o| {
        let o = o.borrow();
        let index = o.get(name).and_then(|v| v.index.clone()).unwrap_or_default();
        (base::run(&index, &base, view_idx, now_ms(), extra.as_deref()), index)
    });
    let width = CONFIG.with(|c| c.borrow().column_width);
    let v = &base.views[view_idx];
    let header = format!(
        "{} · {} ({}){}\n\n",
        source.name(),
        v.name,
        match &v.kind {
            Kind::Table => "table",
            Kind::Cards => "cards",
            Kind::List => "list",
            Kind::Kanban => "kanban",
            Kind::Map => "map",
            Kind::Other(_) => "table",
        },
        extra.as_deref().map(|e| format!(" · filter: {e}")).unwrap_or_default()
    );
    if matches!(v.kind, Kind::Cards | Kind::Kanban | Kind::Map) {
        let cname = format!("bases:{name}");
        // A picture is named relative to its note (entertainment.el's POSTER)
        // or to the vault; a web address is kept as it is (the editor
        // fetches it, if the person allows pictures from the web).
        let image_of = |note: &str, p: &str| {
            let mut p = p.trim();
            // `![[a.png|200]]`, `[[a.png]]`, `![alt](a.png)`, `<a.png>`.
            if let Some(inner) = p.strip_prefix("![[").or_else(|| p.strip_prefix("[[")) {
                p = inner.split("]]").next().unwrap_or(inner);
                p = p.split('|').next().unwrap_or(p);
            } else if let Some((_, rest)) = p.split_once("](") {
                p = rest.split(')').next().unwrap_or(rest);
            }
            let p = p.trim().trim_matches(|c| c == '<' || c == '>' || c == '"');
            let p = p.strip_prefix("file:").unwrap_or(p);
            if p.starts_with("http://") || p.starts_with("https://") {
                return Some(p.to_string());
            }
            if p.is_empty() {
                return None;
            }
            let dir = note.rsplit_once('/').map_or("", |(d, _)| d);
            [format!("{root}/{dir}/{p}"), format!("{root}/{p}"), format!("{root}/attachments/{p}")].into_iter().find(|a| files::exists(a))
        };
        let mut status = String::new();
        OPEN.with(|o| {
            if let Some(open) = o.borrow_mut().get_mut(name) {
                open.lanes.clear();
                open.lane_prop = None;
                if v.kind != Kind::Map {
                    open.map = None;
                }
            }
        });
        let scene = match v.kind {
            Kind::Kanban => {
                let lanes: Vec<String> = base::kanban_columns(&table, v).into_iter().map(|c| c.0).collect();
                let prop = v.group_by.as_ref().map(|g| g.prop.clone());
                OPEN.with(|o| {
                    if let Some(open) = o.borrow_mut().get_mut(name) {
                        open.lanes = lanes;
                        open.lane_prop = prop;
                    }
                });
                if v.group_by.is_none() {
                    status = " · add groupBy to make columns".into();
                }
                let cols = forge_plugin_sdk::environment::frame_width() as f32;
                let width = if cols > 0.0 { cols * 8.5 } else { 1200.0 };
                OPEN.with(|o| {
                    if let Some(open) = o.borrow_mut().get_mut(name) {
                        open.page_w = width;
                    }
                });
                base::kanban_scene(&index, &table, v, width, 600.0)
            }
            Kind::Map => {
                let (pins, missing) = geo::pins(&index, &table, &v.map);
                if missing > 0 {
                    status = format!(" · {missing} without coordinates");
                }
                let mv = MapView::fit(&pins, MAP_W, MAP_H, &v.map);
                let keep = OPEN.with(|o| o.borrow().get(name).and_then(|x| x.map));
                let mut opts = v.map.clone();
                if let Some(t) = &opts.tiles {
                    if !t.starts_with('/') && !t.starts_with('~') {
                        opts.tiles = Some(format!("{root}/{t}"));
                    }
                }
                let view = keep.unwrap_or(mv);
                OPEN.with(|o| {
                    if let Some(open) = o.borrow_mut().get_mut(name) {
                        open.pins = pins.clone();
                        open.map_opts = opts.clone();
                        open.map = Some(view);
                    }
                });
                geo::map_scene(&pins, &view, MAP_W, MAP_H, &opts, &|p| files::exists(p))
            }
            _ => {
                // As wide as the window (a cell is about 8.5 units), so the
                // cards wrap to the room there is: one column on a phone.
                let cols = forge_plugin_sdk::environment::frame_width() as f32;
                let width = if cols > 0.0 { (cols * 8.5).max(base::CARD_W + 32.0) } else { 1200.0 };
                OPEN.with(|o| {
                    if let Some(open) = o.borrow_mut().get_mut(name) {
                        open.page_w = width;
                    }
                });
                base::cards_scene(&index, &table, v, width, &image_of)
            }
        };
        let buf = canvas::show(&cname, &scene.to_json())?;
        let first = OPEN.with(|o| o.borrow().get(name).is_some_and(|v| v.canvas.is_none()));
        if first {
            let n = NEXT.with(|n| {
                *n.borrow_mut() += 1;
                *n.borrow()
            });
            let cmd = format!("bases-event-{n}");
            let for_name = name.to_string();
            command(&cmd, "Handle the pointer on a Bases canvas view (internal).", move |_| {
                canvas_event(&for_name);
                Ok(())
            })?;
            canvas::on_event(&cname, &cmd)?;
        }
        buf.set_major_mode(CARDS_MODE)?;
        // A map moves with the pointer, so the plugin takes over the wheel
        // and drags; the others keep the editor's pan and zoom.
        canvas::handle_view_events(&cname, matches!(v.kind, Kind::Map | Kind::Kanban))?;
        let page_w = OPEN.with(|o| o.borrow().get(name).map_or(0.0, |v| v.page_w));
        canvas::set_scroll(&cname, matches!(v.kind, Kind::Cards | Kind::Kanban), page_w)?;
        if v.kind != Kind::Cards {
            canvas::set_view(&cname, 0.0, 0.0, 1.0)?;
        }
        OPEN.with(|o| {
            if let Some(v) = o.borrow_mut().get_mut(name) {
                v.canvas = Some(cname.clone());
                v.rendered = None;
            }
        });
        canvas::display(&cname)?;
        message(&format!("{} notes{}{}", table.total, status, if table.errors.is_empty() { String::new() } else { format!(" · {}", table.errors[0]) }));
        return Ok(());
    }
    let room = forge_plugin_sdk::environment::frame_width() as usize;
    let budget = (room > 0).then(|| room.saturating_sub(1).max(20));
    let r = if v.kind == Kind::List { base::render_list(&table) } else { base::render_table_fit(&table, width, budget) };
    let header = if r.hidden > 0 { format!("{}  (+{} columns hidden: window too narrow)\n\n", header.trim_end(), r.hidden) } else { header };
    let text = format!("{header}{}", r.text);
    let buf = Buffer::find(name).unwrap_or_else(|| Buffer::create(name));
    buf.set_major_mode(VIEW_MODE)?;
    buf.set_option("read-only", "false")?;
    buf.replace(whole(), &text);
    buf.set_option("read-only", "true")?;
    // Two header lines precede the table's own.
    let shifted = Rendered { text: String::new(), line_notes: std::iter::repeat_n(None, 2).chain(r.line_notes.clone()).collect(), column_starts: r.column_starts.clone(), hidden: r.hidden };
    decorate(&buf, &text, &shifted, v.kind == Kind::List);
    OPEN.with(|o| {
        if let Some(v) = o.borrow_mut().get_mut(name) {
            v.rendered = Some(shifted);
            v.col_props = table.columns.iter().take(r.column_starts.len()).map(|c| c.prop.clone()).collect();
            v.canvas = None;
            v.map = None;
            v.lanes.clear();
            v.lane_prop = None;
        }
    });
    buf.set_point(Position { line: if v.kind == Kind::List { 2 } else { 4 }, column: 0 });
    buf.show();
    Ok(())
}

/// Colour the text views: a dim title line and rule, a bold header, and the
/// note's name underlined as a link, so what you can open stands out.
fn decorate(buf: &Buffer, text: &str, r: &Rendered, list: bool) {
    use forge_plugin_sdk::Decoration;
    let mut out = Vec::new();
    let mut add = |line: usize, start: usize, end: usize, face: &str| {
        if end > start {
            out.push(Decoration {
                span: Span {
                    start: Position { line: line as u32, column: start as u32 },
                    end: Position { line: line as u32, column: end as u32 },
                },
                face: face.to_string(),
                message: None,
            });
        }
    };
    for (i, line) in text.lines().enumerate() {
        let n = line.chars().count();
        let note = r.line_notes.get(i).copied().flatten().is_some();
        if i == 0 {
            add(i, 0, n, "comment");
        } else if note {
            if list {
                let end = line.find(" — ").map_or(n, |b| line[..b].chars().count());
                add(i, 2, end, "link");
            } else {
                let end = r.column_starts.get(1).map_or(n, |s| s.saturating_sub(2)).min(n);
                let title = line.chars().take(end).collect::<String>();
                add(i, 0, title.trim_end().chars().count(), "link");
            }
            if !list {
                // Separators between the other columns stay quiet.
                for s in r.column_starts.iter().skip(1) {
                    add(i, s.saturating_sub(2), *s, "comment");
                }
            }
        } else if line.starts_with('▸') {
            add(i, 0, n, "function");
        } else if line.starts_with('─') {
            add(i, 0, n, "comment");
        } else if !list && i == 2 {
            add(i, 0, n, "keyword");
        } else if line.starts_with('!') {
            add(i, 0, n, "constant");
        } else if line.ends_with("notes") {
            add(i, 0, n, "comment");
        }
    }
    let _ = forge_plugin_sdk::decorations::set(buf, &out);
}

fn canvas_event(buffer_name: &str) {
    let cname = format!("bases:{buffer_name}");
    let Some(json) = canvas::last_event(&cname) else { return };
    let Ok(ev) = serde_json::from_str::<PointerEvent>(&json) else { return };
    let note_of = |id: &Option<String>| id.as_deref().and_then(|id| id.strip_prefix("note:")).and_then(|n| n.parse::<usize>().ok());
    let (kind, is_map, _is_kanban) = OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(buffer_name);
        (ev.kind.clone(), v.is_some_and(|v| v.map.is_some()), v.is_some_and(|v| v.lane_prop.is_some() || !v.lanes.is_empty()))
    });
    match kind.as_str() {
        "press" if ev.button == 3 => {
            OPEN.with(|o| {
                if let Some(v) = o.borrow_mut().get_mut(buffer_name) {
                    v.ctx_note = note_of(&ev.id);
                }
            });
            if let Err(e) = context_menu() {
                message(&e);
            }
        }
        "press" | "double" if ev.button == 1 => {
            // A click opens the note when the button comes up, so a drag
            // that scrolls the page (a finger) doesn't open one.
            OPEN.with(|o| {
                if let Some(v) = o.borrow_mut().get_mut(buffer_name) {
                    v.press = Some((note_of(&ev.id), ev.x, ev.y));
                    v.moved = false;
                }
            });
        }
        "drag" if is_map => {
            let moved = OPEN.with(|o| {
                let mut o = o.borrow_mut();
                let v = o.get_mut(buffer_name)?;
                let (_, px, py) = v.press?;
                v.moved |= (ev.x - px).abs() + (ev.y - py).abs() > 4.0;
                let (dx, dy) = (ev.x - px, ev.y - py);
                if v.moved {
                    v.map.as_mut()?.pan(dx, dy);
                    v.press = Some((None, ev.x, ev.y));
                }
                Some(v.moved)
            });
            if moved == Some(true) {
                redraw_map(buffer_name);
            }
        }
        "drag" => {
            OPEN.with(|o| {
                if let Some(v) = o.borrow_mut().get_mut(buffer_name)
                    && let Some((_, px, py)) = v.press
                {
                    v.moved |= (ev.x - px).abs() + (ev.y - py).abs() > 6.0;
                }
            });
        }
        "release" if ev.button == 1 => {
            let press = OPEN.with(|o| o.borrow_mut().get_mut(buffer_name).and_then(|v| v.press.take().map(|p| (p, v.moved))));
            let Some(((note, ..), moved)) = press else { return };
            let Some(i) = note else { return };
            if is_map {
                if !moved {
                    open_note_index(buffer_name, i);
                }
            } else if !moved {
                open_note_index(buffer_name, i);
            } else {
                let (n_lanes, page_w) = OPEN.with(|o| o.borrow().get(buffer_name).map_or((1, 1200.0), |v| (v.lanes.len(), v.page_w)));
                let (gap, lane_w) = base::kanban_layout(n_lanes, page_w);
                let lane = ((ev.x - gap / 2.0) / (lane_w + gap)).floor();
                if lane >= 0.0 && (lane as usize) < n_lanes {
                    if let Err(e) = move_note(buffer_name, i, lane as usize) {
                        message(&e);
                    }
                }
            }
        }
        "scroll" if is_map => {
            OPEN.with(|o| {
                if let Some(v) = o.borrow_mut().get_mut(buffer_name)
                    && let Some(m) = v.map.as_mut()
                {
                    m.zoom_at(if ev.dy > 0.0 { 0.5 } else { -0.5 }, ev.x, ev.y, MAP_W, MAP_H);
                }
            });
            redraw_map(buffer_name);
        }
        _ => {}
    }
}

/// Draw the map again where it is now looking.
fn redraw_map(name: &str) {
    let Some((cname, scene)) = OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(name)?;
        let view = v.map?;
        Some((v.canvas.clone()?, geo::map_scene(&v.pins, &view, MAP_W, MAP_H, &v.map_opts, &|p| files::exists(p))))
    }) else {
        return;
    };
    let _ = canvas::show(&cname, &scene.to_json());
}

fn yaml_scalar(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.starts_with(|c: char| "-?:,[]{}#&*!|>'\"%@`".contains(c) || c.is_whitespace())
        && !s.contains(": ")
        && !s.contains(" #")
        && !s.ends_with(':')
        && !matches!(s.to_ascii_lowercase().as_str(), "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~");
    if plain { s.to_string() } else { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) }
}

/// Kanban: a card dropped on a lane gets that lane's value for the grouped
/// property, written into the note's front matter.
fn move_note(name: &str, note: usize, lane: usize) -> Result<(), String> {
    let (root, path, prop, value) = OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(name).ok_or("This isn't a Base view")?;
        let prop = v.lane_prop.clone().ok_or("This board has no groupBy property to set")?;
        let value = v.lanes.get(lane).cloned().ok_or("Not over a column")?;
        let path = v.index.as_ref().and_then(|i| i.notes.get(note)).map(|n| n.path.clone()).ok_or("The note is gone")?;
        Ok::<_, String>((v.root.clone(), path, prop, value))
    })?;
    let key = prop.strip_prefix("note.").unwrap_or(&prop);
    if key.starts_with("file.") || key.starts_with("formula.") {
        return Err(format!("Can't move cards by {prop}: it is computed"));
    }
    let full = format!("{root}/{path}");
    let text = files::read(&full)?;
    let org = path.to_lowercase().ends_with(".org");
    let shown = if value.is_empty() { String::new() } else if org { value.clone() } else { yaml_scalar(&value) };
    let new = set_property_in(&path, &text, key, &shown);
    if new != text {
        files::write(&full, new.as_bytes())?;
    }
    message(&format!("{path}: {key} → {}", if value.is_empty() { "(none)" } else { &value }));
    render(name, true)
}

fn map_zoom(steps: f64) -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let v = o.get_mut(&n).ok_or("Not in a Base view")?;
        let m = v.map.as_mut().ok_or("Only map views zoom")?;
        m.zoom_at(steps, MAP_W / 2.0, MAP_H / 2.0, MAP_W, MAP_H);
        Ok::<_, String>(())
    })?;
    redraw_map(&n);
    Ok(())
}

fn map_fit() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let v = o.get_mut(&n).ok_or("Not in a Base view")?;
        v.map.as_ref().ok_or("Only map views fit")?;
        v.map = Some(MapView::fit(&v.pins, MAP_W, MAP_H, &v.map_opts));
        Ok::<_, String>(())
    })?;
    redraw_map(&n);
    Ok(())
}

fn open_note_index(name: &str, i: usize) {
    let Some((root, path)) = OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(name)?;
        let n = v.index.as_ref()?.notes.get(i)?;
        Some((v.root.clone(), n.path.clone()))
    }) else {
        return;
    };
    match Buffer::visit(&format!("{root}/{path}")) {
        Ok(b) => b.show(),
        Err(e) => message(&format!("{path}: {e}")),
    }
}

/// The Base view the current buffer belongs to.
fn current() -> Option<String> {
    let cur = Buffer::current().name();
    if let Some(n) = cur.strip_prefix("*Canvas: bases:").and_then(|n| n.strip_suffix('*')) {
        return OPEN.with(|o| o.borrow().contains_key(n)).then(|| n.to_string());
    }
    OPEN.with(|o| o.borrow().contains_key(&cur)).then_some(cur)
}

fn open_for(source: &Buffer) -> Result<(), String> {
    let path = source.path().ok_or("This Base has no file")?;
    let name = buffer_name(source);
    let root = vault_for(&path);
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let keep = o.remove(&name);
        o.insert(
            name.clone(),
            Open { source: source.id(), root, view: keep.as_ref().map_or(0, |k| k.view), extra: keep.as_ref().and_then(|k| k.extra.clone()), index: keep.and_then(|k| k.index), ..Open::default() },
        );
    });
    render(&name, false)
}

fn view_current() -> Result<(), String> {
    open_for(&Buffer::current())
}

fn refresh() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    render(&n, true)
}

fn step_view(delta: i64) -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let count = OPEN.with(|o| {
        let o = o.borrow();
        let v = &o[&n];
        Buffer::from_id(v.source).and_then(|b| parse_source(&b).ok()).map_or(0, |b| b.views.len())
    });
    if count == 0 {
        return Err("This Base has no views".into());
    }
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let v = o.get_mut(&n).expect("current view");
        v.view = (v.view as i64 + delta).rem_euclid(count as i64) as usize;
        v.sort = None;
    });
    render(&n, false)
}

fn choose_view() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let names: Vec<String> = OPEN
        .with(|o| o.borrow().get(&n).map(|v| v.source))
        .and_then(Buffer::from_id)
        .and_then(|b| parse_source(&b).ok())
        .map(|b| b.views.iter().map(|v| v.name.clone()).collect())
        .unwrap_or_default();
    completing_read("View: ", &names.clone(), true, move |a| {
        let Some(a) = a else { return };
        if let Some(i) = names.iter().position(|x| *x == a) {
            OPEN.with(|o| {
                if let Some(v) = o.borrow_mut().get_mut(&n) {
                    v.view = i;
                    v.sort = None;
                }
            });
            if let Err(e) = render(&n, false) {
                message(&e);
            }
        }
    });
    Ok(())
}

fn ask_filter() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let initial = OPEN.with(|o| o.borrow().get(&n).and_then(|v| v.extra.clone())).unwrap_or_default();
    read_string("Filter (an expression, e.g. status == \"active\"): ", &initial, move |a| {
        let Some(a) = a else { return };
        OPEN.with(|o| {
            if let Some(v) = o.borrow_mut().get_mut(&n) {
                v.extra = (!a.trim().is_empty()).then(|| a.trim().to_string());
            }
        });
        if let Err(e) = render(&n, false) {
            message(&e);
        }
    });
    Ok(())
}

fn clear_filter() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    OPEN.with(|o| o.borrow_mut().get_mut(&n).map(|v| v.extra = None));
    render(&n, false)
}

fn sort_by() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let props: Vec<String> = OPEN
        .with(|o| o.borrow().get(&n).map(|v| v.source))
        .and_then(Buffer::from_id)
        .and_then(|b| parse_source(&b).ok())
        .map(|b| {
            let mut p: Vec<String> = b.views.iter().flat_map(|v| v.order.clone()).collect();
            p.push("file.name".into());
            p.push("file.mtime".into());
            p.sort();
            p.dedup();
            p
        })
        .unwrap_or_default();
    completing_read("Sort by: ", &props, false, move |a| {
        let Some(a) = a.filter(|a| !a.is_empty()) else { return };
        OPEN.with(|o| {
            if let Some(v) = o.borrow_mut().get_mut(&n) {
                // The same property again flips the direction.
                let desc = v.sort.as_ref().is_some_and(|s| s.prop == a && !s.desc);
                v.sort = Some(SortKey { prop: a.clone(), desc });
            }
        });
        if let Err(e) = render(&n, false) {
            message(&e);
        }
    });
    Ok(())
}

/// The note a command means: the one just right-clicked, else the one on
/// point's line.
fn note_here(n: &str) -> Option<usize> {
    let line = Buffer::current().point().line as usize;
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let v = o.get_mut(n)?;
        v.ctx_note.take().or_else(|| v.rendered.as_ref().and_then(|r| r.line_notes.get(line).copied().flatten()))
    })
}

fn note_info(n: &str, i: usize) -> Option<(String, String, String)> {
    OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(n)?;
        let note = v.index.as_ref()?.notes.get(i)?;
        Some((format!("{}/{}", v.root, note.path), note.path.clone(), note.name.clone()))
    })
}

fn open_other() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let i = note_here(&n).ok_or("No note here")?;
    let (full, path, _) = note_info(&n, i).ok_or("The note is gone")?;
    let b = Buffer::visit(&full).map_err(|e| format!("{path}: {e}"))?;
    let _ = run_command("split-window-right");
    let _ = run_command("other-window");
    b.show();
    Ok(())
}

fn copy_link() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let i = note_here(&n).ok_or("No note here")?;
    let (_, path, name) = note_info(&n, i).ok_or("The note is gone")?;
    let link = if path.to_lowercase().ends_with(".org") { format!("[[file:{path}][{name}]]") } else { format!("[[{name}]]") };
    forge_plugin_sdk::kill_ring::push(&link);
    message(&format!("Copied {link}"));
    Ok(())
}

fn copy_path() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let i = note_here(&n).ok_or("No note here")?;
    let (full, ..) = note_info(&n, i).ok_or("The note is gone")?;
    forge_plugin_sdk::kill_ring::push(&full);
    message(&format!("Copied {full}"));
    Ok(())
}

/// Sort by the column point is in (again to reverse).
fn sort_here() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let col = Buffer::current().point().column as usize;
    let prop = OPEN.with(|o| {
        let o = o.borrow();
        let v = o.get(&n)?;
        let starts = &v.rendered.as_ref()?.column_starts;
        let at = starts.iter().rposition(|s| *s <= col).unwrap_or(0);
        v.col_props.get(at).cloned()
    });
    let Some(prop) = prop else { return Err("Not on a column".into()) };
    OPEN.with(|o| {
        if let Some(v) = o.borrow_mut().get_mut(&n) {
            let desc = v.sort.as_ref().is_some_and(|s| s.prop == prop && !s.desc);
            v.sort = Some(SortKey { prop, desc });
        }
    });
    render(&n, false)
}

/// The right-click menu: what can be done to the note under the pointer,
/// then to the view.
fn context_menu() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let note = OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let v = o.get_mut(&n)?;
        // A canvas right-click already named its note.
        if v.canvas.is_some() {
            return v.ctx_note;
        }
        let line = Buffer::current().point().line as usize;
        v.ctx_note = v.rendered.as_ref().and_then(|r| r.line_notes.get(line).copied().flatten());
        v.ctx_note
    });
    let (text_view, filtered) = OPEN.with(|o| o.borrow().get(&n).map_or((false, false), |v| (v.canvas.is_none(), v.extra.is_some())));
    let mut items = Vec::new();
    if note.is_some() {
        items.push(menu_item("Open note", "bases-open-note"));
        items.push(menu_item("Open in other window", "bases-open-note-other"));
        items.push(menu_item("Copy link", "bases-copy-link"));
        items.push(menu_item("Copy file path", "bases-copy-path"));
        items.push(menu_separator());
    }
    if text_view && !is_list_view(&n) {
        items.push(menu_item("Sort by this column", "bases-sort-here"));
    }
    items.push(menu_item("Sort by…", "bases-sort"));
    items.push(menu_item("Filter…", "bases-filter"));
    if filtered {
        items.push(menu_item("Clear filter", "bases-clear-filter"));
    }
    items.push(menu_separator());
    items.push(menu_item("Choose view…", "bases-choose-view"));
    items.push(menu_item("Next view", "bases-next-view"));
    items.push(menu_item("Refresh", "bases-refresh"));
    items.push(menu_separator());
    items.push(menu_item("Edit .base file", "bases-source"));
    items.push(menu_item("Write CSV…", "bases-copy-csv"));
    forge_plugin_sdk::menus::popup(&items)
}

fn is_list_view(n: &str) -> bool {
    OPEN.with(|o| o.borrow().get(n).is_some_and(|v| v.col_props.is_empty()))
}

fn open_at_point() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let note = note_here(&n);
    match note {
        Some(i) => open_note_index(&n, i),
        None => message("No note on this line"),
    }
    Ok(())
}

fn show_source() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    OPEN.with(|o| o.borrow().get(&n).map(|v| v.source)).and_then(Buffer::from_id).ok_or("The .base file's buffer is gone")?.show();
    Ok(())
}

fn copy_csv() -> Result<(), String> {
    let n = current().ok_or("Not in a Base view")?;
    let (source, view, extra, root) = OPEN.with(|o| {
        let o = o.borrow();
        let v = &o[&n];
        (v.source, v.view, v.extra.clone(), v.root.clone())
    });
    let base = parse_source(&Buffer::from_id(source).ok_or("gone")?)?;
    let index = OPEN.with(|o| o.borrow().get(&n).and_then(|v| v.index.clone())).unwrap_or_default();
    let csv = base::to_csv(&base::run(&index, &base, view, now_ms(), extra.as_deref()));
    let target = format!("{root}/{}.csv", n.trim_start_matches("*Base: ").trim_end_matches('*'));
    read_string("Write CSV to: ", &target, move |a| {
        let Some(p) = a.filter(|p| !p.is_empty()) else { return };
        match files::write(&p, csv.as_bytes()) {
            Ok(()) => message(&format!("Wrote {p}")),
            Err(e) => message(&e),
        }
    });
    Ok(())
}

fn new_base() -> Result<(), String> {
    let vault = CONFIG.with(|c| c.borrow().vault.clone()).unwrap_or_else(|| "~/Notes".into());
    read_string("New Base file: ", &files::name_as_directory(&vault), |a| {
        let Some(path) = a.filter(|p| !p.is_empty()) else { return };
        let path = if path.ends_with(".base") { path } else { format!("{path}.base") };
        read_string("Only notes with tag (empty for all): ", "", move |t| {
            let tag = t.map(|t| t.trim().trim_start_matches('#').to_string()).filter(|t| !t.is_empty());
            match Buffer::visit(&path) {
                Ok(b) => {
                    if b.text().trim().is_empty() {
                        b.replace(whole(), &Base::template(tag.as_deref()));
                    }
                    b.show();
                    if let Err(e) = open_for(&b) {
                        message(&format!("bases: {e}"));
                    }
                }
                Err(e) => message(&format!("{path}: {e}")),
            }
        });
    });
    Ok(())
}

fn pick_base() -> Result<(), String> {
    let vault = CONFIG.with(|c| c.borrow().vault.clone()).ok_or("Set the vault first: forge.plugin.config(\"bases\", #{ vault: \"~/Notes\" })")?;
    let found: Vec<String> = files::list_dir(&vault, true)?.into_iter().filter(|p| p.ends_with(".base")).collect();
    if found.is_empty() {
        return Err(format!("No .base files under {vault} (try bases-new)"));
    }
    completing_read("Base: ", &found, true, |a| {
        let Some(p) = a else { return };
        match Buffer::visit(&p) {
            Ok(b) => {
                b.show();
                if let Err(e) = open_for(&b) {
                    message(&format!("bases: {e}"));
                }
            }
            Err(e) => message(&format!("{p}: {e}")),
        }
    });
    Ok(())
}

fn init() -> Result<(), String> {
    let cfg = parse_config(&forge_plugin_sdk::config_json())?;
    CONFIG.with(|c| *c.borrow_mut() = cfg.clone());

    modes::define_major_mode(MODE, Some("fundamental-mode"), &["base".to_string()])?;
    modes::define_major_mode(VIEW_MODE, Some("special-mode"), &[])?;
    modes::define_major_mode(CARDS_MODE, Some("canvas-mode"), &[])?;

    command("bases-view", "Show this .base file as a table, list or cards.", |_| view_current())?;
    command("bases-open", "Choose a .base file in the vault and show it.", |_| pick_base())?;
    command("bases-new", "Make a new .base file with a starter table and cards.", |_| new_base())?;
    command("bases-refresh", "Read the vault and the .base file again.", |_| refresh())?;
    command("bases-next-view", "Show the Base's next view.", |_| step_view(1))?;
    command("bases-previous-view", "Show the Base's previous view.", |_| step_view(-1))?;
    command("bases-choose-view", "Choose one of the Base's views by name.", |_| choose_view())?;
    command("bases-filter", "Narrow the view with an expression.", |_| ask_filter())?;
    command("bases-clear-filter", "Remove the typed filter.", |_| clear_filter())?;
    command("bases-sort", "Sort the view by a property (again to reverse).", |_| sort_by())?;
    command("bases-open-note", "Visit the note on this line.", |_| open_at_point())?;
    command("bases-open-note-other", "Visit the note on this line in another window.", |_| open_other())?;
    command("bases-copy-link", "Copy a link to the note on this line.", |_| copy_link())?;
    command("bases-copy-path", "Copy the file name of the note on this line.", |_| copy_path())?;
    command("bases-sort-here", "Sort by the column point is in (again to reverse).", |_| sort_here())?;
    command("bases-context-menu", "Pop up the menu for what is under the pointer.", |_| context_menu())?;
    command("bases-source", "Show the .base file.", |_| show_source())?;
    command("bases-copy-csv", "Write the view as a CSV file.", |_| copy_csv())?;
    command("bases-export", "Save the cards, board or map view as a picture.", |_| run_command("canvas-export"))?;
    command("bases-zoom-in", "Zoom the map in.", |_| map_zoom(0.5))?;
    command("bases-zoom-out", "Zoom the map out.", |_| map_zoom(-0.5))?;
    command("bases-fit-map", "Zoom the map to show every pin.", |_| map_fit())?;

    for mode in [VIEW_MODE, CARDS_MODE] {
        for (keys, cmd) in [
            ("v", "bases-choose-view"),
            ("]", "bases-next-view"),
            ("[", "bases-previous-view"),
            ("f", "bases-filter"),
            ("F", "bases-clear-filter"),
            ("s", "bases-sort"),
            ("g", "bases-refresh"),
            ("e", "bases-source"),
            ("c", "bases-copy-csv"),
        ] {
            bind_in_mode(mode, keys, cmd)?;
        }
    }
    for (keys, cmd) in [("+", "bases-zoom-in"), ("=", "bases-zoom-in"), ("-", "bases-zoom-out"), ("0", "bases-fit-map")] {
        bind_in_mode(CARDS_MODE, keys, cmd)?;
    }
    bind_in_mode(VIEW_MODE, "RET", "bases-open-note")?;
    bind_in_mode(VIEW_MODE, "<mouse-3>", "bases-context-menu")?;
    bind_in_mode(VIEW_MODE, "S", "bases-sort-here")?;
    bind_in_mode(CARDS_MODE, "<mouse-3>", "bases-context-menu")?;
    bind_in_mode(MODE, "C-c C-c", "bases-view")?;

    let items: Vec<MenuItem> = vec![
        menu_item("Open Base…", "bases-open"),
        menu_item("New Base…", "bases-new"),
        menu_separator(),
        menu_item("Choose view…", "bases-choose-view"),
        menu_item("Next view", "bases-next-view"),
        menu_item("Previous view", "bases-previous-view"),
        menu_separator(),
        menu_item("Filter…", "bases-filter"),
        menu_item("Clear filter", "bases-clear-filter"),
        menu_item("Sort by…", "bases-sort"),
        menu_item("Refresh", "bases-refresh"),
        menu_separator(),
        menu_item("Zoom map in", "bases-zoom-in"),
        menu_item("Zoom map out", "bases-zoom-out"),
        menu_item("Fit map to pins", "bases-fit-map"),
        menu_separator(),
        menu_item("Open note", "bases-open-note"),
        menu_item("Edit .base file", "bases-source"),
        menu_item("Write CSV…", "bases-copy-csv"),
    ];
    define_menu_for_mode("Bases", &items, VIEW_MODE)?;
    define_menu_for_mode("Bases", &items, CARDS_MODE)?;
    define_menu_for_mode("Bases", &[menu_item("Show as view", "bases-view"), menu_item("Open Base…", "bases-open"), menu_item("New Base…", "bases-new")], MODE)?;

    if cfg.auto_view {
        let open = |e: &forge_plugin_sdk::Event| {
            let id = forge_plugin_sdk::event_buffer(e);
            if let Some(b) = Buffer::from_id(id).filter(Buffer::alive)
                && b.major_mode() == MODE
                && Buffer::current().id() == id
                && let Err(e) = open_for(&b)
            {
                message(&format!("bases: {e}"));
            }
        };
        on(EventKind::BufferOpened, open);
    }
    Ok(())
}

forge_plugin!(init);
