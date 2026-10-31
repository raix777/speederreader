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
//! other, the word changing every second (configurable with `--delay`). Each
//! word is drawn as giant block letters, as big as fits while keeping its
//! aspect ratio, centered in the terminal. A chapter summary is available with
//! `--summary`, and the full text with `--text`.
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
/// reference) on the top row, and the current word rendered as giant block
/// letters (an 18-row font baked from Liberation Sans Bold) as big as fits
/// while keeping its aspect ratio, centered in the rest of the terminal.
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

    let initial_speed = 1.0 / delay.as_secs_f64(); // words per second
    let raw_mode = crossterm::terminal::enable_raw_mode().is_ok();
    let mut keys_work = raw_mode;
    // Full-screen display: hide the cursor and disable autowrap (no scrolling).
    let mut term = std::io::stdout();
    term.write_all(b"\x1b[?25l\x1b[?7l").ok();
    term.flush().ok();
    let shown = display_loop(&words, initial_speed, &mut keys_work);
    if raw_mode {
        crossterm::terminal::disable_raw_mode().ok();
    }
    // Restore the terminal and clear the giant letters.
    term.write_all(b"\x1b[?7h\x1b[?25h\x1b[H\x1b[2J").ok();
    term.flush().ok();

    // Summarize on a clean screen.
    if shown == words.len() {
        println!("Done: {} words displayed.", words.len());
    } else {
        println!("Stopped: {} of {} words displayed.", shown, words.len());
    }
}

/// Run the display loop; returns the number of words that were shown.
fn display_loop(words: &[&str], initial_speed: f64, keys_work: &mut bool) -> usize {
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
                draw_frame(words[idx], speed, paused, idx, words.len());
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

/// Assemble a full-screen frame from `lines` (one per row), each placed at
/// column 0 of its own row and padded to `width` cells so the row is fully
/// overwritten (no residue from a longer previous row). `\x1b[H` homes the
/// first line; every later line is prefixed with `\r\n`, whose carriage return
/// returns the cursor to column 0 (a bare `\n` keeps the previous column, which
/// would push the word off to the right and make it vanish). No feed follows
/// the last line, so the screen never scrolls.
fn build_frame(width: usize, lines: &[String]) -> String {
    let mut frame = String::with_capacity(width * lines.len() + 8);
    frame.push_str("\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            frame.push_str("\r\n");
        }
        frame.push_str(line);
        for _ in 0..width.saturating_sub(line.chars().count()) {
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

/// Draw one frame: the status line on the top row and the word as giant block
/// letters, as big as fits while keeping its aspect ratio, centered below it.
fn draw_frame(word: &str, speed: f64, paused: bool, idx: usize, total: usize) {
    let (tw, th) = term_size();
    let area_w = tw.max(8) as usize;
    let area_h = (th.saturating_sub(1)).max(2) as usize;
    let (bw, bmp) = rasterize(word);
    let grid = scale_centered(&bmp, bw, area_w, area_h);

    let mut lines = Vec::with_capacity(area_h + 1);
    lines.push(status_line(speed, paused, false, idx, total));
    lines.extend(grid);
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

/// Rasterize a word with the 18-row block font: each letter is as wide as its
/// advance in the typeface it was baked from, so the word has no extra spacing.
fn rasterize(word: &str) -> (usize, Vec<Vec<bool>>) {
    let mut rows: [Vec<bool>; 18] = Default::default();
    for c in word.chars() {
        let (w, g) = glyph(c);
        for r in 0..18 {
            for b in 0..w.min(32) {
                rows[r].push(g[r] & (1u32 << (31 - b)) != 0);
            }
        }
    }
    (rows[0].len(), rows.into_iter().collect())
}

/// Scale the 18-row glyph bitmap as large as fits in `aw` x `ah` cells while
/// keeping its aspect ratio, and place it centered in a blank grid.
fn scale_centered(bmp: &[Vec<bool>], bw: usize, aw: usize, ah: usize) -> Vec<String> {
    let scale = (aw / bw.max(1)).min(ah / bmp.len()).max(1);
    let word_w = bw * scale;
    let word_h = bmp.len() * scale;
    let left = (aw.saturating_sub(word_w)) / 2;
    let top = (ah.saturating_sub(word_h)) / 2;

    let mut grid: Vec<Vec<char>> = vec![vec![' '; aw]; ah];
    for (r, row) in bmp.iter().enumerate() {
        for (c, px) in row.iter().enumerate() {
            if *px {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let y = top + r * scale + dy;
                        let x = left + c * scale + dx;
                        if y < ah && x < aw {
                            grid[y][x] = '█';
                        }
                    }
                }
            }
        }
    }
    grid.into_iter().map(|row| row.into_iter().collect()).collect()
}

/// The 18-row glyph for a character, baked from Liberation Sans Bold
/// (metric-compatible with Arial): (width in cells, rows with the leftmost
/// pixel in bit 31). Anything not in the table becomes an outline box.
fn glyph(c: char) -> (u8, [u32; 18]) {
    match c {
        ' ' => (3, [0; 18]),
        '!' => (6, [0x00000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x00000000, 0x30000000, 0x30000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '"' => (8, [0x00000000, 0x76000000, 0x76000000, 0x66000000, 0x66000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '#' => (10, [0x00000000, 0x08800000, 0x19800000, 0x11000000, 0x3B800000, 0x7FC00000, 0x33000000, 0x33000000, 0x33000000, 0xFF800000, 0x22000000, 0x66000000, 0x66000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '$' => (10, [0x04000000, 0x1E000000, 0x7F800000, 0x65800000, 0x65000000, 0x7C000000, 0x7E000000, 0x1F800000, 0x05C00000, 0x05C00000, 0xE5C00000, 0x7F800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '%' => (16, [0x00000000, 0x38180000, 0x6C300000, 0x46600000, 0xC6600000, 0xC6C00000, 0x64BE0000, 0x7DB60000, 0x3B620000, 0x02630000, 0x06620000, 0x0C360000, 0x083E0000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '&' => (13, [0x00000000, 0x0F800000, 0x1D800000, 0x18C00000, 0x19800000, 0x1F800000, 0x1E000000, 0x3E600000, 0x76600000, 0x63E00000, 0x63C00000, 0x73E00000, 0x3FF00000, 0x0C100000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '\'' => (4, [0x00000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '(' => (6, [0x08000000, 0x18000000, 0x38000000, 0x30000000, 0x70000000, 0x70000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x70000000, 0x70000000, 0x30000000, 0x30000000, 0x18000000, 0x1C000000, 0x00000000]),
        ')' => (6, [0x40000000, 0x60000000, 0x70000000, 0x30000000, 0x38000000, 0x38000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x38000000, 0x38000000, 0x30000000, 0x70000000, 0x60000000, 0xE0000000, 0x00000000]),
        '*' => (7, [0x00000000, 0x10000000, 0x10000000, 0xFE000000, 0x7C000000, 0x38000000, 0x6C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '+' => (10, [0x00000000, 0x00000000, 0x00000000, 0x0C000000, 0x0C000000, 0x0C000000, 0x7F800000, 0x7FC00000, 0x0E000000, 0x0C000000, 0x0C000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        ',' => (5, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x70000000, 0x70000000, 0x30000000, 0x30000000, 0x20000000, 0x00000000, 0x00000000]),
        '-' => (6, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x78000000, 0x78000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '.' => (5, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '/' => (5, [0x10000000, 0x18000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0xE0000000, 0xC0000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '0' => (10, [0x00000000, 0x1E000000, 0x3F000000, 0x73800000, 0x61800000, 0x61800000, 0x61800000, 0x61800000, 0x61800000, 0x61800000, 0x73800000, 0x33000000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '1' => (10, [0x00000000, 0x0E000000, 0x3E000000, 0x7E000000, 0x4E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x7FC00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '2' => (10, [0x00000000, 0x3F000000, 0x7F800000, 0x73800000, 0x01800000, 0x03800000, 0x07000000, 0x0F000000, 0x1C000000, 0x38000000, 0x70000000, 0x7F800000, 0x7F800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '3' => (10, [0x00000000, 0x3F000000, 0x7F800000, 0x63800000, 0x01800000, 0x03800000, 0x0F000000, 0x0F000000, 0x03800000, 0x01800000, 0x61800000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '4' => (10, [0x00000000, 0x07000000, 0x07000000, 0x0F000000, 0x1B000000, 0x1B000000, 0x33000000, 0x63000000, 0x63000000, 0xFFC00000, 0xFFC00000, 0x03000000, 0x03000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '5' => (10, [0x00000000, 0x7F800000, 0x7F800000, 0x70000000, 0x60000000, 0x7E000000, 0x7F800000, 0x73800000, 0x01800000, 0x01800000, 0x61800000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '6' => (10, [0x00000000, 0x1F000000, 0x3F800000, 0x73800000, 0x60000000, 0x66000000, 0x7F000000, 0x73800000, 0x61800000, 0x61800000, 0x71800000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '7' => (10, [0x00000000, 0x7F800000, 0x7F800000, 0x01800000, 0x03000000, 0x07000000, 0x06000000, 0x0E000000, 0x0C000000, 0x1C000000, 0x18000000, 0x18000000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '8' => (10, [0x00000000, 0x3F000000, 0x7F800000, 0x73800000, 0x61800000, 0x73800000, 0x3F000000, 0x3F000000, 0x61800000, 0x61800000, 0x61800000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '9' => (10, [0x00000000, 0x3E000000, 0x7F000000, 0x73800000, 0x61800000, 0x61800000, 0x73800000, 0x7F800000, 0x3D800000, 0x01800000, 0x23800000, 0x77000000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        ':' => (6, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x30000000, 0x30000000, 0x30000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x30000000, 0x30000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        ';' => (6, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x30000000, 0x30000000, 0x30000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x30000000, 0x30000000, 0x10000000, 0x10000000, 0x30000000, 0x00000000, 0x00000000]),
        '<' => (10, [0x00000000, 0x00000000, 0x00000000, 0x01C00000, 0x07C00000, 0x3E000000, 0x78000000, 0x60000000, 0x78000000, 0x3F000000, 0x07C00000, 0x00C00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '=' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x7FC00000, 0x7FC00000, 0x00000000, 0x00000000, 0x00000000, 0x7FC00000, 0x7FC00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '>' => (10, [0x00000000, 0x00000000, 0x00000000, 0x60000000, 0x7C000000, 0x1F000000, 0x03C00000, 0x01C00000, 0x07C00000, 0x1F000000, 0x78000000, 0x60000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '?' => (11, [0x00000000, 0x1F000000, 0x3F800000, 0x71C00000, 0x61C00000, 0x01C00000, 0x03800000, 0x07000000, 0x0E000000, 0x0C000000, 0x00000000, 0x0C000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '@' => (17, [0x00000000, 0x03F80000, 0x0C060000, 0x18030000, 0x31E90000, 0x23390000, 0x66190000, 0x64110000, 0x44110000, 0x44310000, 0x64330000, 0x67DE0000, 0x23980000, 0x30000000, 0x1C1C0000, 0x07F00000, 0x00000000, 0x00000000]),
        'A' => (13, [0x00000000, 0x07000000, 0x0F800000, 0x0F800000, 0x0D800000, 0x1DC00000, 0x18C00000, 0x18C00000, 0x3FE00000, 0x3FE00000, 0x30600000, 0x70700000, 0x60300000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'B' => (13, [0x00000000, 0x7FC00000, 0x7FE00000, 0x70E00000, 0x70600000, 0x70E00000, 0x7FC00000, 0x7FE00000, 0x70700000, 0x70300000, 0x70700000, 0x7FF00000, 0x7FE00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'C' => (13, [0x00000000, 0x0FC00000, 0x3FE00000, 0x38700000, 0x70300000, 0x70000000, 0x60000000, 0x60000000, 0x60000000, 0x70200000, 0x70700000, 0x3CF00000, 0x1FE00000, 0x03000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'D' => (13, [0x00000000, 0x7F800000, 0x7FC00000, 0x70E00000, 0x70700000, 0x70700000, 0x70300000, 0x70300000, 0x70700000, 0x70700000, 0x70E00000, 0x7FE00000, 0x7FC00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'E' => (12, [0x00000000, 0x7FE00000, 0x7FE00000, 0x70000000, 0x70000000, 0x70000000, 0x7FC00000, 0x7FC00000, 0x70000000, 0x70000000, 0x70000000, 0x7FE00000, 0x7FE00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'F' => (11, [0x00000000, 0x7FC00000, 0x7FC00000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x7FC00000, 0x7FC00000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'G' => (14, [0x00000000, 0x0FC00000, 0x3FF00000, 0x38700000, 0x70200000, 0x70000000, 0x60000000, 0x61F80000, 0x60F80000, 0x70180000, 0x70380000, 0x3CF80000, 0x1FE00000, 0x03000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'H' => (13, [0x00000000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x7FF00000, 0x7FF00000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'I' => (5, [0x00000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'J' => (10, [0x00000000, 0x0F800000, 0x0F800000, 0x03800000, 0x03800000, 0x03800000, 0x03800000, 0x03800000, 0x03800000, 0x03800000, 0xE3800000, 0x77000000, 0x7F000000, 0x1C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'K' => (13, [0x00000000, 0x70700000, 0x70E00000, 0x71C00000, 0x73800000, 0x77000000, 0x7E000000, 0x7F000000, 0x73800000, 0x71C00000, 0x70E00000, 0x70E00000, 0x70700000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'L' => (11, [0x00000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x7FC00000, 0x7FC00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'M' => (15, [0x00000000, 0x783C0000, 0x783C0000, 0x783C0000, 0x7C7C0000, 0x7C7C0000, 0x7C7C0000, 0x76DC0000, 0x76DC0000, 0x77DC0000, 0x739C0000, 0x739C0000, 0x739C0000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'N' => (13, [0x00000000, 0x70700000, 0x78700000, 0x7C700000, 0x7C700000, 0x6E700000, 0x76700000, 0x77700000, 0x73700000, 0x71F00000, 0x71F00000, 0x70F00000, 0x70F00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'O' => (14, [0x00000000, 0x0FC00000, 0x3FF00000, 0x38700000, 0x70380000, 0x70380000, 0x60180000, 0x60180000, 0x60380000, 0x70380000, 0x70300000, 0x3CF00000, 0x1FE00000, 0x03000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'P' => (12, [0x00000000, 0x7F800000, 0x7FE00000, 0x70E00000, 0x70600000, 0x70600000, 0x70E00000, 0x7FC00000, 0x7F800000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'Q' => (14, [0x00000000, 0x0FC00000, 0x3FF00000, 0x38700000, 0x70380000, 0x70380000, 0x60180000, 0x60180000, 0x60380000, 0x70380000, 0x70380000, 0x3CF00000, 0x1FE00000, 0x03800000, 0x03800000, 0x01F00000, 0x00F00000, 0x00000000]),
        'R' => (13, [0x00000000, 0x7FC00000, 0x7FE00000, 0x70700000, 0x70700000, 0x70700000, 0x70F00000, 0x7FE00000, 0x7FC00000, 0x71C00000, 0x70E00000, 0x70600000, 0x70700000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'S' => (12, [0x00000000, 0x1F800000, 0x3FC00000, 0x70E00000, 0x70000000, 0x78000000, 0x3F800000, 0x0FC00000, 0x01E00000, 0x00600000, 0x60600000, 0x79E00000, 0x3FC00000, 0x06000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'T' => (11, [0x00000000, 0xFFE00000, 0xFFE00000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x0E000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'U' => (13, [0x00000000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x70700000, 0x3DE00000, 0x1FC00000, 0x07000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'V' => (12, [0x00000000, 0xE0700000, 0x60600000, 0x70E00000, 0x70E00000, 0x30C00000, 0x39C00000, 0x19800000, 0x19800000, 0x1D800000, 0x0F000000, 0x0F000000, 0x0F000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'W' => (17, [0x00000000, 0xE1C38000, 0xE1C38000, 0x63E30000, 0x63E30000, 0x73670000, 0x73670000, 0x33760000, 0x36360000, 0x3E3E0000, 0x3E3E0000, 0x1E3C0000, 0x1C3C0000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'X' => (12, [0x00000000, 0x70E00000, 0x30E00000, 0x39C00000, 0x19800000, 0x0F800000, 0x0F000000, 0x0F000000, 0x1F800000, 0x19800000, 0x39C00000, 0x70E00000, 0x60600000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'Y' => (12, [0x00000000, 0x60600000, 0x70E00000, 0x30C00000, 0x39C00000, 0x1F800000, 0x0F000000, 0x0F000000, 0x06000000, 0x06000000, 0x06000000, 0x06000000, 0x06000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'Z' => (11, [0x00000000, 0x7FC00000, 0x7FC00000, 0x01C00000, 0x03800000, 0x07000000, 0x06000000, 0x0C000000, 0x1C000000, 0x38000000, 0x70000000, 0x7FC00000, 0x7FC00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '[' => (6, [0x78000000, 0x7C000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x78000000, 0x7C000000, 0x00000000]),
        '\\' => (5, [0x40000000, 0xC0000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x60000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x38000000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        ']' => (6, [0x78000000, 0xF8000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x18000000, 0x78000000, 0xF8000000, 0x00000000]),
        '^' => (10, [0x00000000, 0x0E000000, 0x1E000000, 0x1B000000, 0x33000000, 0x31800000, 0x61800000, 0x60C00000, 0x60C00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '_' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0xFFC00000, 0x00000000, 0x00000000]),
        '`' => (6, [0x60000000, 0x30000000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'a' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3F000000, 0x73000000, 0x63800000, 0x03800000, 0x3F800000, 0x73800000, 0x63800000, 0x67800000, 0x7DC00000, 0x10800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'b' => (11, [0x20000000, 0x70000000, 0x70000000, 0x70000000, 0x7F800000, 0x7BC00000, 0x71C00000, 0x70C00000, 0x70C00000, 0x70C00000, 0x71C00000, 0x79C00000, 0x7F800000, 0x02000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'c' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x1F000000, 0x3B800000, 0x71800000, 0x60000000, 0x60000000, 0x60000000, 0x71800000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'd' => (11, [0x00800000, 0x01C00000, 0x01C00000, 0x01C00000, 0x3FC00000, 0x7BC00000, 0x71C00000, 0x61C00000, 0x61C00000, 0x61C00000, 0x71C00000, 0x73C00000, 0x3FC00000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'e' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3F000000, 0x73800000, 0x71800000, 0x71800000, 0x7F800000, 0x60000000, 0x60000000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'f' => (6, [0x0C000000, 0x3C000000, 0x30000000, 0x30000000, 0xFC000000, 0x7C000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x30000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'g' => (11, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3FC00000, 0x7BC00000, 0x71C00000, 0x61C00000, 0x61C00000, 0x61C00000, 0x71C00000, 0x7BC00000, 0x3FC00000, 0x01C00000, 0x21C00000, 0x7B800000, 0x3F000000, 0x00000000]),
        'h' => (11, [0x20000000, 0x70000000, 0x70000000, 0x70000000, 0x77800000, 0x7FC00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'i' => (5, [0x20000000, 0x70000000, 0x00000000, 0x00000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'j' => (5, [0x20000000, 0x70000000, 0x00000000, 0x00000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0xF0000000, 0xE0000000, 0x00000000]),
        'k' => (10, [0x20000000, 0x70000000, 0x70000000, 0x70000000, 0x71800000, 0x73000000, 0x76000000, 0x7E000000, 0x7E000000, 0x77000000, 0x73000000, 0x73800000, 0x71800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'l' => (5, [0x20000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'm' => (16, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x773C0000, 0x7FFE0000, 0x71CE0000, 0x718E0000, 0x718E0000, 0x718E0000, 0x718E0000, 0x718E0000, 0x718E0000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'n' => (11, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x77800000, 0x7FC00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'o' => (11, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x1F000000, 0x3B800000, 0x71C00000, 0x60C00000, 0x60C00000, 0x60C00000, 0x71C00000, 0x71C00000, 0x3F800000, 0x0E000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'p' => (11, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x7F800000, 0x7BC00000, 0x71C00000, 0x70C00000, 0x70C00000, 0x70C00000, 0x71C00000, 0x79C00000, 0x7F800000, 0x72000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000]),
        'q' => (11, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3FC00000, 0x7BC00000, 0x71C00000, 0x61C00000, 0x61C00000, 0x61C00000, 0x71C00000, 0x73C00000, 0x3FC00000, 0x19C00000, 0x01C00000, 0x01C00000, 0x01C00000, 0x00000000]),
        'r' => (7, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x7E000000, 0x7E000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        's' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x3F000000, 0x73800000, 0x60000000, 0x78000000, 0x3F000000, 0x0F800000, 0x01800000, 0x61800000, 0x7F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        't' => (6, [0x00000000, 0x00000000, 0x30000000, 0x30000000, 0xFC000000, 0x78000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x3C000000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'u' => (11, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x73C00000, 0x3FC00000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'v' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0xE1C00000, 0x61800000, 0x63800000, 0x73800000, 0x33000000, 0x33000000, 0x3E000000, 0x1E000000, 0x1E000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'w' => (14, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0xC30C0000, 0xE79C0000, 0x67980000, 0x67980000, 0x6CD80000, 0x6CF80000, 0x3CF00000, 0x3CF00000, 0x38700000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'x' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x61800000, 0x73800000, 0x3F000000, 0x1E000000, 0x1E000000, 0x1E000000, 0x3F000000, 0x73800000, 0x61800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'y' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0xE1C00000, 0x61800000, 0x71800000, 0x33800000, 0x33000000, 0x3B000000, 0x1E000000, 0x1E000000, 0x0E000000, 0x0C000000, 0x1C000000, 0x78000000, 0x70000000, 0x00000000]),
        'z' => (9, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x7F000000, 0x7F000000, 0x06000000, 0x0E000000, 0x1C000000, 0x38000000, 0x30000000, 0x70000000, 0x7F000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '{' => (7, [0x0C000000, 0x1E000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x30000000, 0xE0000000, 0x70000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x3C000000, 0x1E000000, 0x00000000]),
        '|' => (5, [0x20000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x70000000, 0x00000000]),
        '}' => (7, [0x60000000, 0xF0000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x18000000, 0x0E000000, 0x1C000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x38000000, 0x78000000, 0xF0000000, 0x00000000]),
        '~' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x78000000, 0x7FC00000, 0x07800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'ä' => (10, [0x00000000, 0x33000000, 0x33000000, 0x00000000, 0x3F000000, 0x73000000, 0x63800000, 0x03800000, 0x7F800000, 0x63800000, 0xE3800000, 0x67800000, 0x7DC00000, 0x10800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'ö' => (11, [0x00000000, 0x1B000000, 0x11000000, 0x00000000, 0x1F000000, 0x3B800000, 0x71C00000, 0x60C00000, 0x60C00000, 0x60C00000, 0x71C00000, 0x71C00000, 0x3F800000, 0x0E000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'ü' => (11, [0x00000000, 0x13000000, 0x13000000, 0x00000000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x71C00000, 0x73C00000, 0x3FC00000, 0x18000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'ß' => (11, [0x0E000000, 0x3F800000, 0x39800000, 0x71800000, 0x71800000, 0x73000000, 0x77000000, 0x77000000, 0x73C00000, 0x71C00000, 0x70C00000, 0x70C00000, 0x77C00000, 0x03000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        'é' => (10, [0x03000000, 0x06000000, 0x0C000000, 0x00000000, 0x3F000000, 0x73800000, 0x71800000, 0x71800000, 0x7F800000, 0x60000000, 0x60000000, 0x73800000, 0x3F000000, 0x0C000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '‘' => (5, [0x00000000, 0x20000000, 0x60000000, 0x60000000, 0x70000000, 0x70000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '’' => (5, [0x00000000, 0x70000000, 0x70000000, 0x30000000, 0x30000000, 0x20000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '“' => (9, [0x00000000, 0x32000000, 0x26000000, 0x66000000, 0x77000000, 0x77000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '”' => (9, [0x00000000, 0x77000000, 0x77000000, 0x33000000, 0x32000000, 0x22000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '–' => (10, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x7F800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        '—' => (18, [0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0xFFFFC000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000]),
        _ => (10, [0x3FFC0000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x80400000, 0x3FFC0000]),
    }
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

    /// (top, bottom, left, right) of the inked cells, or None if the grid is blank.
    fn ink_bbox(grid: &[String]) -> Option<(usize, usize, usize, usize)> {
        let (mut top, mut bottom) = (usize::MAX, 0usize);
        let (mut left, mut right) = (usize::MAX, 0usize);
        for (y, row) in grid.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                if ch == '█' {
                    top = top.min(y);
                    bottom = bottom.max(y);
                    left = left.min(x);
                    right = right.max(x);
                }
            }
        }
        if top == usize::MAX {
            None
        } else {
            Some((top, bottom, left, right))
        }
    }

    #[test]
    fn every_word_fits_the_terminal_and_is_drawn() {
        for len in [1usize, 2, 5, 10, 20, 50, 200] {
            let word = "a".repeat(len);
            let (bw, bmp) = rasterize(&word);
            let grid = scale_centered(&bmp, bw, 80, 23);
            assert_eq!(grid.len(), 23, "word of length {len}");
            for (i, row) in grid.iter().enumerate() {
                assert_eq!(row.chars().count(), 80, "row {i}, word of length {len}");
            }
            assert!(
                grid.iter().any(|r| r.contains('█')),
                "word of length {len} was not drawn"
            );
        }
    }

    #[test]
    fn the_font_is_18_rows_tall_and_letters_keep_their_own_widths() {
        let (bw, bmp) = rasterize("A");
        assert_eq!(bmp.len(), 18, "every row of the font");
        assert!(bw > 0);
        assert!(bmp.iter().any(|row| row.iter().any(|&px| px)), "'A' has ink");
        // Wide and narrow letters must differ (real advances, not a fixed cell).
        let (ww, _) = rasterize("W");
        let (wi, _) = rasterize("i");
        assert!(ww > wi, "W ({ww}) should be wider than i ({wi})");
    }

    #[test]
    fn an_18_row_word_fits_and_centers_in_a_tall_area() {
        // Synthetic full-ink bitmap: the ink bbox then equals the bitmap bbox.
        let bmp = vec![vec![true; 10]; 18];
        let grid = scale_centered(&bmp, 10, 80, 23);
        assert_eq!(grid.len(), 23);
        let (top, bottom, _, _) = ink_bbox(&grid).expect("word not drawn");
        assert_eq!((top, bottom), (2, 19), "18 rows centered in 23");
    }

    #[test]
    fn a_full_height_word_is_centered_both_ways() {
        let bmp = vec![vec![true; 14]; 18];
        let grid = scale_centered(&bmp, 14, 80, 23);
        let (top, bottom, left, right) = ink_bbox(&grid).expect("word not drawn");
        assert!(left.abs_diff(80 - right - 1) <= 1, "left {left} right {right}");
        assert!(top.abs_diff(23 - bottom - 1) <= 1, "top {top} bottom {bottom}");
    }

    #[test]
    fn a_very_long_word_shrinks_to_fit_instead_of_overflowing() {
        let word = "x".repeat(200);
        let (bw, bmp) = rasterize(&word);
        let grid = scale_centered(&bmp, bw, 80, 23);
        let (_, _, left, right) = ink_bbox(&grid).expect("word not drawn");
        assert!(right < 80, "ink overflows the right edge: right {right}");
        assert!(right >= 80 - 5, "ink stops before the right edge: right {right}");
        assert!(left <= 2, "a 200-letter word starts at the left edge (left {left})");
    }

    #[test]
    fn build_frame_puts_each_line_at_column_zero_pads_to_width_and_feeds_nothing_after_the_last() {
        // A bare \n moves down a row but keeps the column, so a line would be
        // drawn off to the right of column 0 and vanish; \r returns the cursor
        // to column 0. Padding to `width` clears a longer previous row, and no
        // trailing feed keeps the screen from scrolling.
        let lines = vec!["ab".to_string(), "abc".to_string()];
        assert_eq!(build_frame(5, &lines), "\x1b[Hab   \r\nabc  ");
    }
}
