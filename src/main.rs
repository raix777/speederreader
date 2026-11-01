//! speederreader — load an EPUB and report its reading structure.
//!
//! An EPUB is a ZIP archive with a small, well-defined layout:
//!
//! ```text
//! mimetype                    "application/epub+zip" (must be first + stored)
//! META-INF/container.xml      points at the OPF package document
//! <dir>/content.opf           metadata + manifest (files) + spine (reading order)
//! <dir>/*.xhtml               the actual readable content
//! ```
//!
//! By default this program opens the archive, walks that layout, extracts the
//! readable text of each spine item in reading order, and shows the book as a
//! rapid-serial display (RSVP): the words appear in the terminal one after the
//! other, the word changing every second (configurable with `--delay`).
//!
//! Each word is rendered with an anti-aliased serif (a bundled DejaVu Serif
//! rasterized to a grayscale bitmap and drawn with half-blocks), and balanced
//! around a fixed vertical center line: a vowel near the middle of the word is
//! picked as the recognition point (ORP), highlighted in red and placed on the
//! center line, with the rest of the word dimmed by its distance from it. A
//! chapter summary is available with `--summary`, and the full text with
//! `--text`.
//!
//! While the display runs, `+`/`=` increase the speed by 5 words/s, `-`
//! decreases it, `space` pauses/resumes, `p` pauses and shows the last 30 words
//! plus the current word in the normal font, and `q` quits.

use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyModifiers};
use zip::ZipArchive;

type BoxedError = Box<dyn Error>;

/// A loaded EPUB book.
#[derive(Debug)]
struct Epub {
    title: String,
    language: String,
    author: String,
    chapters: Vec<Chapter>,
}

/// One reading unit: an XHTML document referenced from the spine.
#[derive(Debug)]
struct Chapter {
    path: String,
    title: String,
    words: usize,
    text: String,
}

impl Epub {
    /// Load and parse an EPUB file from disk.
    fn load(path: &Path) -> Result<Self, BoxedError> {
        let file = File::open(path)
            .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
        let mut zip = ZipArchive::new(file).map_err(|e| e.to_string())?;

        // 1) Verify it is actually an EPUB (spec: mimetype must be first/stored).
        let mimetype = read_entry(&mut zip, "mimetype")?;
        if mimetype.trim() != "application/epub+zip" {
            return Err(format!("not a valid EPUB (mimetype: {:?})", mimetype.trim()).into());
        }

        // 2) container.xml -> path of the OPF package document.
        let container = read_entry(&mut zip, "META-INF/container.xml")?;
        let opf_rel = opf_path_from_container(&container)?;
        let opf_dir = parent_dir(&opf_rel);

        // 3) Parse the OPF: metadata + manifest (id -> href) + spine (idref order).
        let opf = read_entry(&mut zip, &opf_rel)?;
        let (title, language, author, manifest, spine) = parse_opf(&opf)?;

        // 4) Walk the spine in reading order and load each content document.
        let mut chapters = Vec::new();
        for idref in &spine {
            let Some(href) = manifest.get(idref) else {
                eprintln!("warning: spine itemref '{}' not in manifest; skipped", idref);
                continue;
            };
            let content_path = join_paths(&opf_dir, href);
            let content = read_entry(&mut zip, &content_path)?;
            let text = extract_text(&content);
            let title = heading_from(&content)
                .or_else(|| {
                    href.rsplit('/')
                        .next()
                        .map(|s| s.trim_end_matches(".xhtml").trim_end_matches(".html").to_string())
                })
                .unwrap_or_else(|| href.clone());
            chapters.push(Chapter {
                path: href.clone(),
                title,
                words: count_words(&text),
                text,
            });
        }

        Ok(Epub {
            title,
            language,
            author,
            chapters,
        })
    }
}

fn main() {
    let mut print_text = false;
    let mut summary = false;
    let mut read_mode = false;
    let mut delay = std::time::Duration::from_secs(1);
    let mut path: Option<String> = None;
    let mut skip_next = false;

    for a in std::env::args().skip(1) {
        if skip_next {
            if let Ok(v) = a.parse::<f64>() {
                if v > 0.0 {
                    delay = std::time::Duration::from_secs_f64(v);
                }
            }
            skip_next = false;
            continue;
        }
        match a.as_str() {
            "--text" | "-t" => print_text = true,
            "--summary" | "-s" => summary = true,
            "--read" | "-r" | "--rsvp" => read_mode = true,
            "--delay" => skip_next = true,
            other if !other.starts_with('-') => path = Some(other.to_string()),
            _ => {}
        }
    }
    let path = path.unwrap_or_else(|| "dummy.epub".to_string());

    let epub = match Epub::load(Path::new(&path)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    // The default view is the word display; the summary is opt-in.
    if read_mode || (!print_text && !summary) {
        rsvp_display(&epub, delay);
        return;
    }

    println!("Loaded:     {}", path);
    println!("Title:      {}", epub.title);
    if epub.author != "Unknown" {
        println!("Author:     {}", epub.author);
    }
    println!("Language:   {}", epub.language);
    println!("Chapters:   {}", epub.chapters.len());
    println!();

    let mut total = 0usize;
    for (i, ch) in epub.chapters.iter().enumerate() {
        println!("[{}] {}", i + 1, ch.title);
        println!("    file:  {}", ch.path);
        println!("    words: {}", ch.words);
        total += ch.words;
    }
    println!();
    println!("Total words: {}", total);

    if print_text {
        println!();
        println!("===== text =====");
        for ch in &epub.chapters {
            println!("\n--- {} ---", ch.title);
            println!("{}", ch.text);
        }
    }
}

/// Speed change per `+`/`-` press, in words per second.
const SPEED_STEP: f64 = 5.0;
/// Slowest allowed speed (5 s per word).
const MIN_SPEED: f64 = 0.2;

/// Rapid-serial display: show the book's words in the terminal, one after
/// the other. The initial word pace is `delay` (default: 1 s per word).
///
/// The display is full-screen: the status line (speed, progress, key
/// reference) on the top row, and the current word rendered with an
/// anti-aliased serif and balanced around a fixed vertical center line — a
/// vowel near the middle (the ORP) in red on the line, the rest dimmed by
/// distance. The current pace in words per minute is shown bottom-right.
/// Keys while running:
///
/// | key                    | effect                         |
/// |------------------------|--------------------------------|
/// | `+` or `=`             | speed up by 5 words/s          |
/// | `-`                    | slow down by 5 words/s         |
/// | `space`                | pause / resume                 |
/// | `p`                    | pause + show the last 30 words |
/// |                        | and the current word in the    |
/// |                        | normal font (again: resume)    |
/// | `q`, `Esc`, `Ctrl+C`   | quit                           |
///
/// If the terminal does not support raw mode (e.g. stdin is not a TTY), the
/// display runs without key control.
fn rsvp_display(epub: &Epub, delay: Duration) {
    let words: Vec<&str> = epub
        .chapters
        .iter()
        .flat_map(|ch| ch.text.split_whitespace())
        .collect();

    if words.is_empty() {
        eprintln!("nothing to display: no words found");
        return;
    }

    let font = match load_font() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let initial_speed = 1.0 / delay.as_secs_f64(); // words per second
    let raw_mode = crossterm::terminal::enable_raw_mode().is_ok();
    let mut keys_work = raw_mode;
    // Full-screen display: hide the cursor and disable autowrap (no scrolling).
    let mut term = std::io::stdout();
    term.write_all(b"\x1b[?25l\x1b[?7l").ok();
    term.flush().ok();
    let shown = display_loop(&font, &words, initial_speed, &mut keys_work);
    if raw_mode {
        crossterm::terminal::disable_raw_mode().ok();
    }
    // Restore the terminal: re-enable autowrap, show the cursor, reset color,
    // and clear the word.
    term.write_all(b"\x1b[?7h\x1b[?25h\x1b[0m\x1b[H\x1b[2J").ok();
    term.flush().ok();

    // Summarize on a clean screen.
    if shown == words.len() {
        println!("Done: {} words displayed.", words.len());
    } else {
        println!("Stopped: {} of {} words displayed.", shown, words.len());
    }
}

/// Run the display loop; returns the number of words that were shown.
fn display_loop(font: &Font, words: &[&str], initial_speed: f64, keys_work: &mut bool) -> usize {
    let step = |speed: f64| Duration::from_secs_f64(1.0 / speed);

    let mut speed = initial_speed;
    let mut paused = false;
    let mut review = false; // paused + showing the last 30 words in normal font
    let mut idx = 0;
    let mut next_change = Instant::now() + step(speed);
    let mut dirty = true;

    loop {
        // Redraw the whole screen whenever the word, speed, or pause state
        // changed. One buffered write per frame, so the screen does not flicker.
        if dirty {
            if review {
                draw_review_frame(words, speed, idx, words.len());
            } else {
                draw_frame(font, words[idx], speed, paused, idx, words.len());
            }
            dirty = false;
        }

        // Advance to the next word once its time is up (unless paused/reviewing).
        if !paused && !review && Instant::now() >= next_change {
            idx += 1;
            if idx >= words.len() {
                return idx;
            }
            next_change = Instant::now() + step(speed);
            dirty = true;
            continue;
        }

        // Wait for the next word change, or for a key press in the meantime.
        // (No extra margin: the deadline is re-checked at the top of the loop.)
        let timeout = if paused || review {
            Duration::from_millis(50)
        } else {
            next_change.saturating_duration_since(Instant::now())
        };

        if *keys_work {
            match crossterm::event::poll(timeout) {
                Ok(true) => {
                    if let Some(Event::Key(key)) = crossterm::event::read().ok() {
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            return idx; // Ctrl+C quits, like before raw mode
                        }
                        match key.code {
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                speed += SPEED_STEP;
                                next_change = Instant::now() + step(speed);
                            }
                            KeyCode::Char('-') | KeyCode::Char('_') => {
                                speed = (speed - SPEED_STEP).max(MIN_SPEED);
                                next_change = Instant::now() + step(speed);
                            }
                            KeyCode::Char(' ') => {
                                if review {
                                    review = false; // leave the review view, resume
                                } else {
                                    paused = !paused;
                                }
                                next_change = Instant::now() + step(speed);
                            }
                            KeyCode::Char('p') => {
                                review = !review;
                                next_change = Instant::now() + step(speed);
                            }
                            KeyCode::Char('q') | KeyCode::Esc => {
                                return idx;
                            }
                            _ => {}
                        }
                        dirty = true;
                    }
                }
                Ok(false) => {}
                Err(_) => *keys_work = false, // lost the terminal; keep showing
            }
        } else {
            std::thread::sleep(timeout);
        }
    }
}

/// The status line: speed, progress, and a short key reference (or pause hint).
fn status_line(speed: f64, paused: bool, review: bool, idx: usize, total: usize) -> String {
    let progress = format!("[{idx:5}/{}]", total);
    if review {
        format!(
            "PAUSED (review) — p to resume   |   speed {speed:.1} words/s   {progress}"
        )
    } else if paused {
        format!("PAUSED — space to resume   |   speed {speed:.1} words/s   {progress}")
    } else {
        format!(
            "speed {speed:.1} words/s ({per:.2} s/word)   {progress}   +/- speed   space pause   p review   q quit",
            per = 1.0 / speed
        )
    }
}

/// Terminal size, falling back to 80x24 when unknown (also for a 0x0 pty).
fn term_size() -> (u16, u16) {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    if w == 0 || h == 0 {
        (80, 24)
    } else {
        (w, h)
    }
}

/// Number of terminal cells a line occupies, ignoring SGR color escapes
/// (`\x1b[...m`), so padding can clear a row without counting escape bytes.
fn visible_len(s: &str) -> usize {
    let mut cells = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&p) = chars.peek() {
                chars.next();
                if p == 'm' {
                    break;
                }
            }
        } else {
            cells += 1;
        }
    }
    cells
}

/// Assemble a full-screen frame from `lines` (one per row), each placed at
/// column 0 of its own row and padded to `width` cells so the row is fully
/// overwritten (no residue from a longer previous row). `\x1b[H` homes the
/// first line and `\x1b[0m` resets the color to a clean slate; every later line
/// is prefixed with `\r\n`, whose carriage return returns the cursor to column
/// 0 (a bare `\n` keeps the previous column, which would push the word off to
/// the right and make it vanish). No feed follows the last line, so the screen
/// never scrolls. Padding uses the visible length so color escapes do not
/// skew it.
fn build_frame(width: usize, lines: &[String]) -> String {
    let mut frame = String::with_capacity(width * lines.len() + 16);
    frame.push_str("\x1b[H\x1b[0m");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            frame.push_str("\r\n");
        }
        frame.push_str(line);
        for _ in 0..width.saturating_sub(visible_len(line)) {
            frame.push(' ');
        }
    }
    frame
}

/// Write a frame to stdout and flush it (one buffered write per frame, so the
/// screen does not flicker).
fn write_frame(width: usize, lines: &[String]) {
    let mut out = std::io::stdout();
    out.write_all(build_frame(width, lines).as_bytes()).ok();
    out.flush().ok();
}

/// Truncate a plain (color-free) string to at most `width` cells.
fn truncate(s: &str, width: usize) -> String {
    s.chars().take(width).collect()
}

/// Draw one frame: the status line on the top row, and the current word
/// rendered with the anti-aliased serif and balanced around the vertical center
/// line (ORP in red, rest dimmed), with the pace in words/minute bottom-right.
fn draw_frame(font: &Font, word: &str, speed: f64, paused: bool, idx: usize, total: usize) {
    let (tw, th) = term_size();
    let area_w = tw.max(8) as usize;
    let area_h = (th.saturating_sub(1)).max(2) as usize;
    let wpm = (speed * 60.0).round() as i64;
    let status = truncate(&status_line(speed, paused, false, idx, total), area_w);

    let lines = match rasterize_word(font, word, fit_em(font, word, area_w, area_h)) {
        Some(raster) => {
            let orp = orp_index(word);
            let mut lines = vec![status];
            lines.extend(word_frame(&raster, orp, area_w, area_h, wpm));
            lines
        }
        // No ink (e.g. a word of characters the font cannot draw): show blanks.
        None => {
            let mut lines = vec![status];
            lines.resize(area_h + 1, String::new());
            lines
        }
    };
    write_frame(area_w, &lines);
}

/// Draw the review frame (`p` key): the status line, the current word, and
/// the last 30 words, all in the normal terminal font instead of the giant
/// block letters.
fn draw_review_frame(words: &[&str], speed: f64, idx: usize, total: usize) {
    const TRAIL_WORDS: usize = 30;
    let (tw, th) = term_size();
    let width = tw.max(8) as usize;
    let height = (th.saturating_sub(1)).max(1) as usize;

    let start = idx.saturating_sub(TRAIL_WORDS);
    let mut rows: Vec<String> = Vec::new();
    rows.push(format!("current: {}", words[idx]));
    rows.push(format!("last {} words:", idx - start));
    rows.extend(wrap_text(&words[start..idx].join(" "), width));

    let mut lines = Vec::with_capacity(height + 1);
    lines.push(status_line(speed, true, true, idx, total));
    // Exactly `height` content rows (blanks pad the tail); `build_frame`
    // clears each one to the full width.
    for r in 0..height {
        lines.push(rows.get(r).cloned().unwrap_or_default());
    }
    write_frame(width, &lines);
}

/// Word-wrap `text` so every line fits in at most `width` columns.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut rows: Vec<String> = Vec::new();
    let mut line = String::new();
    for w in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(w);
        } else if line.chars().count() + 1 + w.chars().count() <= width {
            line.push(' ');
            line.push_str(w);
        } else {
            rows.push(std::mem::take(&mut line));
            line.push_str(w);
        }
    }
    if !line.is_empty() {
        rows.push(line);
    }
    rows
}

/// Pixels per terminal cell, vertically (cells are ~2:1, so the width is 2x).
/// A larger value gives finer anti-aliasing.
const CELL_PX: usize = 8;
/// Letters within this many of the ORP stay full-bright; the rest dim away.
const ORP_NEAR: usize = 1;
/// Brightness lost per step away from the ORP, and the dim floor.
const DIM_STEP: f64 = 0.12;
const DIM_MIN: f64 = 0.40;
/// Height of the center-line tick at the top and bottom of the word area.
const TICK_LEN: usize = 3;
type Rgb = (u8, u8, u8);
/// One output cell: a glyph with a foreground and a background color.
type Cell = Option<(char, Rgb, Rgb)>;
const BLACK: Rgb = (0, 0, 0);
const ORP_RED: Rgb = (220, 45, 45);
const WORD_WHITE: Rgb = (245, 245, 245);
const TICK_GRAY: Rgb = (110, 110, 110);
const WPM_GRAY: Rgb = (150, 150, 150);
const FONT_BYTES: &[u8] = include_bytes!("../assets/dejavuserif.ttf");

/// A serif face, loaded once and shared across frames.
struct Font(rusttype::Font<'static>);

/// Load the bundled DejaVu Serif (Bitstream Vera / DejaVu license; see
/// assets/README).
fn load_font() -> Result<Font, BoxedError> {
    rusttype::Font::try_from_vec(FONT_BYTES.to_vec())
        .map(Font)
        .ok_or_else(|| "failed to load the embedded font".into())
}

/// The index of the recognition point (ORP) in `word`: the vowel that best
/// balances the word around its centre, or the middle character when the word
/// has no vowel. This is the letter highlighted in red on the center line.
fn orp_index(word: &str) -> usize {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n == 0 {
        return 0;
    }
    let balance = |i: usize| i.abs_diff(n - 1 - i);
    let mut best: Option<usize> = None;
    for (i, &c) in chars.iter().enumerate() {
        if is_vowel(c) && best.map_or(true, |b| balance(i) < balance(b)) {
            best = Some(i);
        }
    }
    best.unwrap_or(n / 2)
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'A' | 'E' | 'I' | 'O' | 'U')
}

/// Relative brightness of a letter by its distance from the ORP: full near the
/// ORP, fading toward a dim floor at the word's edges.
fn letter_bright(i: usize, orp: usize) -> f64 {
    let d = i.abs_diff(orp);
    if d <= ORP_NEAR {
        1.0
    } else {
        (1.0 - DIM_STEP * (d - ORP_NEAR) as f64).max(DIM_MIN)
    }
}

/// One word rasterized from the font: per-pixel coverage (0..1) with the top of
/// the ink at row 0, its pixel size, and each character's advance range in
/// pixels as (start, end).
struct RasterWord {
    cover: Vec<f32>,
    w: usize,
    h: usize,
    ranges: Vec<(f32, f32)>,
}

/// Total advance width of `word` in pixels at em size `em`.
fn word_advance(font: &Font, word: &str, em: f32) -> f32 {
    let scale = rusttype::Scale::uniform(em);
    word.chars()
        .map(|c| font.0.glyph(c).scaled(scale).h_metrics().advance_width)
        .sum()
}

/// The em size (pixels) that makes the word about two-fifths of the area tall
/// and no wider than the area, shrinking long words to fit.
fn fit_em(font: &Font, word: &str, area_w: usize, area_h: usize) -> f32 {
    let desired = (((area_h * 2) / 5).max(2) * CELL_PX) as f32;
    let cells_w = (word_advance(font, word, desired) / (2 * CELL_PX) as f32).ceil() as usize;
    let max_cells = (area_w - 2).max(2);
    if cells_w > max_cells {
        desired * max_cells as f32 / cells_w as f32
    } else {
        desired
    }
}

/// Rasterize `word` at em size `em` into a coverage bitmap. Returns `None` when
/// the word has no ink (e.g. every character is unrenderable).
fn rasterize_word(font: &Font, word: &str, em: f32) -> Option<RasterWord> {
    let scale = rusttype::Scale::uniform(em);
    let mut ranges = Vec::new();
    let mut pen = 0f32;
    let (mut top, mut bottom) = (f32::MAX, f32::MIN);
    let mut inked = false;
    for c in word.chars() {
        let advance = font.0.glyph(c).scaled(scale).h_metrics().advance_width;
        ranges.push((pen, pen + advance));
        if let Some(bb) = font
            .0
            .glyph(c)
            .scaled(scale)
            .positioned(rusttype::Point { x: pen, y: 0.0 })
            .pixel_bounding_box()
        {
            inked = true;
            top = top.min(bb.min.y as f32);
            bottom = bottom.max(bb.max.y as f32);
        }
        pen += advance;
    }
    if !inked {
        return None;
    }
    let w = pen.ceil().max(1.0) as usize;
    let top_px = top.floor() as i32;
    let h = (bottom.ceil() as i32 - top_px).max(1) as usize;
    let mut cover = vec![0f32; w * h];
    pen = 0.0;
    for c in word.chars() {
        let advance = font.0.glyph(c).scaled(scale).h_metrics().advance_width;
        let placed = font
            .0
            .glyph(c)
            .scaled(scale)
            .positioned(rusttype::Point { x: pen, y: 0.0 });
        if let Some(bb) = placed.pixel_bounding_box() {
            placed.draw(|x, y, v| {
                let cx = (bb.min.x as i32 + x as i32) as usize;
                let cy = bb.min.y as i32 + y as i32 - top_px;
                if cy >= 0 {
                    let cy = cy as usize;
                    if cx < w && cy < h {
                        let i = cy * w + cx;
                        cover[i] = cover[i].max(v);
                    }
                }
            });
        }
        pen += advance;
    }
    Some(RasterWord { cover, w, h, ranges })
}

/// Downsample a coverage bitmap into per-cell (top, bottom) values with half
/// blocks: each cell averages its two vertical halves, so one cell can show two
/// shades (double the vertical resolution, for smoother edges).
fn downsample(cover: &[f32], w: usize, h: usize) -> (Vec<Vec<[f32; 2]>>, usize, usize) {
    let (block_w, block_h) = (2 * CELL_PX, CELL_PX);
    let cw = (w + block_w - 1) / block_w;
    let ch = (h + block_h - 1) / block_h;
    let mut grid: Vec<Vec<[f32; 2]>> = vec![vec![[0.0, 0.0]; cw]; ch];
    for cy in 0..ch {
        let r0 = cy * block_h;
        let r1 = (r0 + block_h).min(h);
        let rm = r0 + ((r1 - r0) / 2).max(1);
        for cx in 0..cw {
            let (c0, c1) = (cx * block_w, (cx * block_w + block_w).min(w));
            let (mut tsum, mut bsum) = (0f64, 0f64);
            for y in r0..r1 {
                for x in c0..c1 {
                    let v = cover[y * w + x] as f64;
                    if y < rm {
                        tsum += v;
                    } else {
                        bsum += v;
                    }
                }
            }
            let tcount = (rm - r0) * (c1 - c0);
            let bcount = ((r1 - rm) * (c1 - c0)).max(1);
            grid[cy][cx] = [
                (tsum / tcount as f64) as f32,
                (bsum / bcount as f64) as f32,
            ];
        }
    }
    (grid, cw, ch)
}

/// The character whose advance range covers pixel column `px`.
fn char_at(ranges: &[(f32, f32)], px: f32) -> usize {
    ranges
        .iter()
        .position(|&(_, end)| px < end)
        .unwrap_or_else(|| ranges.len().saturating_sub(1))
}

/// The left offset (in cells) that places the ORP on the center column.
fn word_left_offset(orp_center_px: f32, area_w: usize) -> i64 {
    (area_w / 2) as i64 - (orp_center_px / (2 * CELL_PX) as f32).round() as i64
}

/// The word area as `area_h` lines of exactly `area_w` cells: the serif word
/// balanced on the center line (ORP in red, rest dimmed by distance), a
/// center-line tick top and bottom, and the pace in words/minute bottom-right.
fn word_frame(
    raster: &RasterWord,
    orp: usize,
    area_w: usize,
    area_h: usize,
    wpm: i64,
) -> Vec<String> {
    let (grid, _cw, ch) = downsample(&raster.cover, raster.w, raster.h);
    let center_col = area_w / 2;
    let left = word_left_offset((raster.ranges[orp].0 + raster.ranges[orp].1) / 2.0, area_w);
    let top = (area_h.saturating_sub(ch) / 2) as i64;

    let mut out: Vec<Vec<Cell>> = vec![vec![None; area_w]; area_h];
    paint_ticks(&mut out, area_w, area_h, center_col);
    paint_word(&mut out, &grid, orp, &raster.ranges, left, top, area_w, area_h);
    paint_wpm(&mut out, area_w, area_h, wpm);
    out.iter().map(|row| row_to_line(row)).collect()
}

/// Mark the center-line tick at the top and bottom of the area.
fn paint_ticks(out: &mut Vec<Vec<Cell>>, area_w: usize, area_h: usize, center_col: usize) {
    if area_w == 0 || area_h == 0 || center_col >= area_w {
        return;
    }
    for r in 0..TICK_LEN.min(area_h) {
        out[r][center_col] = Some(('│', TICK_GRAY, BLACK));
        out[area_h - 1 - r][center_col] = Some(('│', TICK_GRAY, BLACK));
    }
}

/// Paint the word's half-block cells, each letter tinted by its distance from
/// the ORP (the ORP letter in red).
fn paint_word(
    out: &mut Vec<Vec<Cell>>,
    grid: &[Vec<[f32; 2]>],
    orp: usize,
    ranges: &[(f32, f32)],
    left: i64,
    // rows of (top, bottom) coverage, one row per output cell
    top: i64,
    area_w: usize,
    area_h: usize,
) {
    for (cy, row) in grid.iter().enumerate() {
        for (cx, [tv, bv]) in row.iter().enumerate() {
            if *tv < 0.02 && *bv < 0.02 {
                continue;
            }
            let (ty, tx) = (top + cy as i64, left + cx as i64);
            if ty < 0 || tx < 0 || ty >= area_h as i64 || tx >= area_w as i64 {
                continue;
            }
            let px = cx as f32 * (2 * CELL_PX) as f32 + CELL_PX as f32;
            let ci = char_at(ranges, px);
            let base = if ci == orp {
                ORP_RED
            } else {
                let b = letter_bright(ci, orp) as f32;
                (
                    (WORD_WHITE.0 as f32 * b) as u8,
                    (WORD_WHITE.1 as f32 * b) as u8,
                    (WORD_WHITE.2 as f32 * b) as u8,
                )
            };
            out[ty as usize][tx as usize] = Some(half_block(*tv, *bv, base));
        }
    }
}

/// Paint the pace (words/minute) in the bottom-right corner.
fn paint_wpm(out: &mut Vec<Vec<Cell>>, area_w: usize, area_h: usize, wpm: i64) {
    if area_w == 0 || area_h == 0 {
        return;
    }
    let label = format!("{wpm} wpm");
    let start = (area_w - 1).saturating_sub(label.chars().count());
    for (i, c) in label.chars().enumerate() {
        out[area_h - 1][start + i] = Some((c, WPM_GRAY, BLACK));
    }
}

/// Choose a half-block character and its two shades so the cell shows `top`
/// coverage on top and `bottom` below, tinted by `base`.
fn half_block(top: f32, bottom: f32, base: Rgb) -> (char, Rgb, Rgb) {
    if top >= bottom {
        ('▀', shade(base, top), shade(base, bottom))
    } else {
        ('▄', shade(base, bottom), shade(base, top))
    }
}

/// A base color scaled to a coverage level (0..1) — the anti-aliased edge gray.
fn shade(c: Rgb, v: f32) -> Rgb {
    let v = v.clamp(0.0, 1.0);
    ((c.0 as f32 * v) as u8, (c.1 as f32 * v) as u8, (c.2 as f32 * v) as u8)
}

/// A true-color foreground+background SGR sequence.
fn sgr(fg: Rgb, bg: Rgb) -> String {
    format!(
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
    )
}

/// Serialize one row of cells to a line: a reset at the start, then each colored
/// glyph with an SGR only when its (fg, bg) changes, and a reset before any
/// blank run that follows a colored cell.
fn row_to_line(row: &[Cell]) -> String {
    let mut line = String::new();
    line.push_str("\x1b[0m");
    let mut cur: Option<(Rgb, Rgb)> = None;
    for cell in row {
        match cell {
            None => {
                if cur.is_some() {
                    line.push_str("\x1b[0m");
                    cur = None;
                }
                line.push(' ');
            }
            Some((ch, fg, bg)) => {
                if cur != Some((*fg, *bg)) {
                    line.push_str(&sgr(*fg, *bg));
                    cur = Some((*fg, *bg));
                }
                line.push(*ch);
            }
        }
    }
    line
}
// --- EPUB internals ---------------------------------------------------------

/// Read one archive entry as a lossy UTF-8 string.
fn read_entry<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String, BoxedError> {
    let mut f = zip
        .by_name(name)
        .map_err(|e| format!("missing or unreadable entry '{}': {}", name, e))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| format!("failed to read '{}': {}", name, e))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Find the OPF path in META-INF/container.xml (`<rootfile full-path="..."/>`).
fn opf_path_from_container(container: &str) -> Result<String, BoxedError> {
    let doc = roxmltree::Document::parse(container).map_err(|e| format!("bad container.xml: {}", e))?;
    for node in doc.root().descendants() {
        if node.has_tag_name("rootfile") {
            if let Some(p) = node.attribute("full-path") {
                return Ok(p.to_string());
            }
        }
    }
    Err("no <rootfile full-path> found in container.xml".into())
}

/// Parse the OPF into (title, language, author, manifest, spine).
///
/// roxmltree matches on *local* names, so `<dc:title>` matches `"title"`.
fn parse_opf(
    opf: &str,
) -> Result<(String, String, String, HashMap<String, String>, Vec<String>), BoxedError> {
    let doc = roxmltree::Document::parse(opf).map_err(|e| format!("bad OPF: {}", e))?;
    let mut title = String::from("Unknown");
    let mut language = String::from("und");
    let mut author = String::from("Unknown");
    let mut manifest: HashMap<String, String> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();

    for node in doc.root().descendants() {
        match node.tag_name().name() {
            "title" if title == "Unknown" => {
                if let Some(t) = first_text(&node) {
                    title = t;
                }
            }
            "language" if language == "und" => {
                if let Some(t) = first_text(&node) {
                    language = t;
                }
            }
            "creator" if author == "Unknown" => {
                if let Some(t) = first_text(&node) {
                    author = t;
                }
            }
            "item" => {
                if let (Some(id), Some(href)) = (node.attribute("id"), node.attribute("href")) {
                    manifest.insert(id.to_string(), href.to_string());
                }
            }
            "itemref" => {
                if let Some(idref) = node.attribute("idref") {
                    spine.push(idref.to_string());
                }
            }
            _ => {}
        }
    }

    Ok((title, language, author, manifest, spine))
}

// --- text extraction --------------------------------------------------------

/// Extract readable text (in document order) from an XHTML document.
fn extract_text(xhtml: &str) -> String {
    let doc = match roxmltree::Document::parse(xhtml) {
        Ok(d) => d,
        Err(_) => return normalize(&strip_tags(xhtml)),
    };
    let root = doc.root();
    let body = root.descendants().find(|n| n.has_tag_name("body"));
    let raw = match body {
        Some(b) => render_into(&b),
        None => render_into(&root),
    };
    normalize(&raw)
}

/// First human-readable title in a content document (`<h1>`/`<h2>`/`<h3>`).
fn heading_from(xhtml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xhtml).ok()?;
    for n in doc.root().descendants() {
        if matches!(n.tag_name().name(), "h1" | "h2" | "h3") {
            if let Some(t) = first_text(&n) {
                return Some(t);
            }
        }
    }
    None
}

/// First non-empty text within `node` (direct text child, then nested).
fn first_text(node: &roxmltree::Node) -> Option<String> {
    if let Some(t) = node.text() {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    for d in node.descendants() {
        if let Some(t) = d.text() {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Render an element subtree to plain text, adding a line break after blocks.
fn render_into(node: &roxmltree::Node) -> String {
    let mut out = String::new();
    push_render(node, &mut out);
    out
}

fn push_render(node: &roxmltree::Node, out: &mut String) {
    for child in node.children() {
        if child.is_element() {
            push_render(&child, out);
        } else if child.is_text() {
            if let Some(t) = child.text() {
                let t = t.trim();
                if !t.is_empty() {
                    out.push_str(t);
                    out.push(' ');
                }
            }
        }
    }
    if is_block(node.tag_name().name()) {
        out.push('\n');
    }
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "div" | "section" | "article" | "li"
            | "ul" | "ol" | "blockquote" | "pre" | "table" | "tr" | "td" | "th" | "body"
            | "header" | "footer" | "main"
    )
}

/// Collapse intra-line whitespace, drop empty lines, separate blocks by a blank line.
fn normalize(s: &str) -> String {
    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Minimal tag stripper, used only if the document is not well-formed XML.
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn count_words(s: &str) -> usize {
    s.split_whitespace().count()
}

// --- tiny path helpers for archive-internal paths --------------------------

fn parent_dir(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

/// Join an OPF directory with a (possibly relative) manifest href, resolving `.`/`..`.
fn join_paths(base: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = base
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    for p in relative.split('/') {
        if p.is_empty() || p == "." {
            continue;
        }
        if p == ".." {
            parts.pop();
        } else {
            parts.push(p);
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_orp_is_the_best_balanced_vowel() {
        assert_eq!(orp_index("seeing"), 2); // e
        assert_eq!(orp_index("minute."), 3); // u
        assert_eq!(orp_index("recreate"), 4); // e
        assert_eq!(orp_index("aeiou"), 2); // i, dead centre
        assert_eq!(orp_index("the"), 2); // e
        assert_eq!(orp_index("a"), 0);
        assert_eq!(orp_index("ab"), 0); // a is the only vowel
    }

    #[test]
    fn the_orp_falls_back_to_the_middle_when_there_is_no_vowel() {
        assert_eq!(orp_index("rhythm"), 3);
        assert_eq!(orp_index("bc"), 1);
        assert_eq!(orp_index("xyz"), 1);
    }

    #[test]
    fn the_orp_always_stays_within_the_word() {
        for word in ["a", "aa", "ab", "gym", "hello", "beautiful", &"x".repeat(40)] {
            let i = orp_index(word);
            assert!(i < word.chars().count(), "{word:?} -> {i}");
        }
    }

    #[test]
    fn brightness_falls_off_away_from_the_orp() {
        assert!((letter_bright(4, 4) - 1.0).abs() < 1e-9);
        assert!((letter_bright(5, 4) - 1.0).abs() < 1e-9);
        assert!(letter_bright(6, 4) < 1.0);
        assert!(letter_bright(6, 4) > letter_bright(9, 4)); // 0.88 > 0.52
        assert!((letter_bright(100, 4) - DIM_MIN).abs() < 1e-9); // floor
    }

    #[test]
    fn downsample_averages_a_cell_and_its_two_halves() {
        let (w, h) = (2 * CELL_PX, CELL_PX);
        let mut cover = vec![0f32; w * h];
        for y in 0..h / 2 {
            for x in 0..w {
                cover[y * w + x] = 1.0;
            }
        }
        let (grid, cw, ch) = downsample(&cover, w, h);
        assert_eq!((cw, ch), (1, 1));
        assert!((grid[0][0][0] - 1.0).abs() < 1e-6);
        assert!(grid[0][0][1] < 1e-6);
    }

    #[test]
    fn char_at_finds_the_range_that_covers_a_pixel() {
        let ranges = [(0.0, 10.0), (10.0, 25.0), (25.0, 40.0)];
        assert_eq!(char_at(&ranges, 0.0), 0);
        assert_eq!(char_at(&ranges, 9.9), 0);
        assert_eq!(char_at(&ranges, 10.0), 1);
        assert_eq!(char_at(&ranges, 39.0), 2);
    }

    #[test]
    fn a_half_block_picks_the_heavier_half_for_the_glyph() {
        let (ch, fg, bg) = half_block(1.0, 0.0, (100, 100, 100));
        assert_eq!(ch, '▀');
        assert_eq!(fg, (100, 100, 100));
        assert_eq!(bg, (0, 0, 0));
        let (ch2, fg2, bg2) = half_block(0.0, 1.0, (100, 100, 100));
        assert_eq!(ch2, '▄');
        assert_eq!(fg2, (100, 100, 100));
        assert_eq!(bg2, (0, 0, 0));
    }

    #[test]
    fn the_left_offset_centers_the_orp_on_the_center_column() {
        for area_w in [80usize, 63usize] {
            for c in 0..20i64 {
                let orp_px = (c * (2 * CELL_PX) as i64) as f32;
                let left = word_left_offset(orp_px, area_w);
                assert_eq!(left + c, (area_w / 2) as i64);
            }
        }
    }

    #[test]
    fn visible_len_ignores_color_escapes() {
        assert_eq!(visible_len("ab\x1b[38;2;255;0;0mX"), 3);
        assert_eq!(visible_len("\x1b[0m hello"), 6);
        assert_eq!(visible_len("plain"), 5);
    }

    #[test]
    fn build_frame_homes_resets_and_pads_by_visible_length() {
        let lines = vec![
            "ab\x1b[38;2;255;0;0mX".to_string(), // visible: abX (3 cells)
            "abc".to_string(),
        ];
        assert_eq!(
            build_frame(5, &lines),
            "\x1b[H\x1b[0mab\x1b[38;2;255;0;0mX  \r\nabc  "
        );
    }

    // Render a word through the full (font) pipeline into an 80x24 word area.
    fn render(word: &str, area_w: usize, area_h: usize) -> Vec<String> {
        let font = load_font().unwrap();
        let em = fit_em(&font, word, area_w, area_h);
        let raster = rasterize_word(&font, word, em).expect("word should render");
        word_frame(&raster, orp_index(word), area_w, area_h, 300)
    }

    /// The visible character and its colors at column `col` of an ANSI line.
    fn cell_at(line: &str, col: usize) -> Option<(char, Rgb, Rgb)> {
        let mut cells = 0usize;
        let (mut fg, mut bg) = (BLACK, BLACK);
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                let mut buf = String::new();
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p == 'm' {
                        break;
                    }
                    buf.push(p);
                }
                let params: Vec<u32> = buf.split(';').filter_map(|p| p.parse().ok()).collect();
                let mut i = 0usize;
                while i < params.len() {
                    match params[i] {
                        38 if params.get(i + 1) == Some(&2) => {
                            fg = (
                                params.get(i + 2).copied().unwrap_or(0) as u8,
                                params.get(i + 3).copied().unwrap_or(0) as u8,
                                params.get(i + 4).copied().unwrap_or(0) as u8,
                            );
                            i += 5;
                        }
                        48 if params.get(i + 1) == Some(&2) => {
                            bg = (
                                params.get(i + 2).copied().unwrap_or(0) as u8,
                                params.get(i + 3).copied().unwrap_or(0) as u8,
                                params.get(i + 4).copied().unwrap_or(0) as u8,
                            );
                            i += 5;
                        }
                        0 => {
                            (fg, bg) = (BLACK, BLACK);
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
                continue;
            }
            if cells == col {
                return Some((c, fg, bg));
            }
            cells += 1;
        }
        None
    }

    #[test]
    fn the_word_frame_has_the_right_shape() {
        let lines = render("seeing", 80, 24);
        assert_eq!(lines.len(), 24);
        for line in &lines {
            assert_eq!(visible_len(line), 80, "every row spans the width");
        }
    }

    #[test]
    fn the_orp_letter_is_reddish_on_the_center_column() {
        let lines = render("minute.", 80, 24);
        let center: usize = 80 / 2;
        let found = lines.iter().any(|line| {
            (center.saturating_sub(1)..=center + 1).any(|col| {
                cell_at(line, col)
                    .map_or(false, |(_, fg, _)| fg.0 > 120 && fg.0 > fg.1 && fg.0 > fg.2)
            })
        });
        assert!(found, "the ORP letter is not reddish near the center column");
    }

    #[test]
    fn the_center_tick_and_pace_are_drawn() {
        let lines = render("minute.", 80, 24);
        assert_eq!(cell_at(&lines[0], 40).map(|(c, _, _)| c), Some('│'));
        assert!(strip_ansi(&lines[23]).trim_end().ends_with("300 wpm"));
    }

    #[test]
    fn words_of_many_lengths_fit_the_area() {
        let font = load_font().unwrap();
        for len in [1usize, 2, 5, 10, 30] {
            let word = "a".repeat(len);
            let em = fit_em(&font, &word, 80, 24);
            let raster = rasterize_word(&font, &word, em).unwrap();
            let lines = word_frame(&raster, orp_index(&word), 80, 24, 100);
            assert_eq!(lines.len(), 24);
            for line in &lines {
                assert_eq!(visible_len(line), 80);
            }
        }
    }

    #[test]
    fn a_word_rasterizes_to_an_inked_bitmap() {
        let font = load_font().unwrap();
        let r = rasterize_word(&font, "seeing", 40.0).unwrap();
        assert!(r.w > 0 && r.h > 0);
        assert!(r.cover.iter().any(|&v| v > 0.5), "the word has solid ink");
        assert_eq!(r.ranges.len(), 6, "one advance range per letter");
    }

    #[test]
    fn a_long_word_shrinks_to_fit_the_width() {
        let font = load_font().unwrap();
        let word = "x".repeat(60);
        let em = fit_em(&font, &word, 80, 24);
        let cells = (word_advance(&font, &word, em) / (2 * CELL_PX) as f32).ceil() as usize;
        assert!(cells <= 78, "a 60-letter word must fit 80 cols (got {cells})");
    }

    #[test]
    fn a_word_without_ink_is_skipped() {
        let font = load_font().unwrap();
        assert!(rasterize_word(&font, "   ", 40.0).is_none());
    }

    /// Strip SGR color escapes, for readable assertions.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
