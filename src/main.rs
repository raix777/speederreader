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
//! This program opens the archive, walks that layout, extracts the readable
//! text of each spine item in reading order, and prints a summary (or the full
//! text with `--text`).
//!
//! With `--read` it becomes a rapid-serial display (RSVP): the words of the
//! book are shown in the terminal one after the other, the word changes every
//! second (configurable with `--delay`). Each word is drawn as giant block
//! letters that fill the screen. While running, `+`/`=` increase the speed by
//! 5 words/s, `-` decreases it, `space` pauses/resumes, `q` quits.

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

    if read_mode {
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
/// letters (5x7 font, stretched to fill the rest of the terminal). Keys
/// while running:
///
/// | key                    | effect                         |
/// |------------------------|--------------------------------|
/// | `+` or `=`             | speed up by 5 words/s          |
/// | `-`                    | slow down by 5 words/s         |
/// | `space`                | pause / resume                 |
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
    let mut idx = 0;
    let mut next_change = Instant::now() + step(speed);
    let mut dirty = true;

    loop {
        // Redraw the whole screen whenever the word, speed, or pause state
        // changed. One buffered write per frame, so the screen does not flicker.
        if dirty {
            draw_frame(words[idx], speed, paused, idx, words.len());
            dirty = false;
        }

        // Advance to the next word once its time is up (unless paused).
        if !paused && Instant::now() >= next_change {
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
        let timeout = if paused {
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
                                paused = !paused;
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
fn status_line(speed: f64, paused: bool, idx: usize, total: usize) -> String {
    let progress = format!("[{idx:5}/{}]", total);
    if paused {
        format!("PAUSED — space to resume   |   speed {speed:.1} words/s   {progress}")
    } else {
        format!(
            "speed {speed:.1} words/s ({per:.2} s/word)   {progress}   +/- speed   space pause   q quit",
            per = 1.0 / speed
        )
    }
}

/// Draw one frame: the status line on the top row and the word as giant
/// block letters filling the rest of the terminal.
fn draw_frame(word: &str, speed: f64, paused: bool, idx: usize, total: usize) {
    let (tw, th) = crossterm::terminal::size().unwrap_or((80, 24));
    let area_w = tw.max(8) as usize;
    let area_h = (th.saturating_sub(1)).max(2) as usize;
    let (bw, bmp) = rasterize(word);
    let screen = stretch(&bmp, bw, area_w, area_h);

    let mut frame = String::with_capacity(area_w * area_h + 128);
    frame.push_str("\x1b[H"); // home: overwrite the previous frame from the top
    frame.push_str(&status_line(speed, paused, idx, total));
    frame.push('\n');
    for row in &screen {
        for px in row {
            frame.push(if *px { '█' } else { ' ' });
        }
        frame.push('\n');
    }
    let mut out = std::io::stdout();
    out.write_all(frame.as_bytes()).ok();
    out.flush().ok();
}

/// Rasterize a word with the 5x7 block font: 7 rows, 6px per character
/// (5px glyph + 1px spacing).
fn rasterize(word: &str) -> (usize, Vec<Vec<bool>>) {
    let chars: Vec<char> = word.chars().collect();
    let width = if chars.is_empty() { 1 } else { 6 * chars.len() - 1 };
    let mut bmp = vec![vec![false; width]; 7];
    for (i, c) in chars.iter().enumerate() {
        let g = glyph(*c);
        for r in 0..7 {
            for b in 0..5u8 {
                if g[r] & (1 << (4 - b)) != 0 {
                    bmp[r][i * 6 + b as usize] = true;
                }
            }
        }
    }
    (width, bmp)
}

/// Nearest-neighbor stretch of the 7-row bitmap to exactly `aw` x `ah` cells,
/// so the word always fills its whole area (aspect ratio is not preserved).
fn stretch(bmp: &[Vec<bool>], bw: usize, aw: usize, ah: usize) -> Vec<Vec<bool>> {
    let bw = bw.max(1);
    (0..ah)
        .map(|r| {
            let src_r = r * 7 / ah;
            (0..aw).map(|c| bmp[src_r][c * bw / aw]).collect()
        })
        .collect()
}

/// The 5x7 glyph for a character (bit 4 = leftmost pixel). ASCII plus common
/// accented letters; anything else becomes an outline box.
fn glyph(c: char) -> [u8; 7] {
    match c {
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
        'E' => [0b11111, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        'a' => [0, 0, 0b01110, 0b00001, 0b01110, 0b10001, 0b01111],
        'b' => [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110],
        'c' => [0, 0, 0b01110, 0b10000, 0b10000, 0b10001, 0b01110],
        'd' => [0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111],
        'e' => [0, 0, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        'f' => [0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000],
        'g' => [0, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'h' => [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
        'i' => [0b00100, 0, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
        'j' => [0b00010, 0, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100],
        'k' => [0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010],
        'l' => [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'm' => [0, 0, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001],
        'n' => [0, 0, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
        'o' => [0, 0, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
        'p' => [0, 0, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000],
        'q' => [0, 0, 0b01101, 0b10011, 0b01111, 0b00001, 0b00001],
        'r' => [0, 0, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000],
        's' => [0, 0, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
        't' => [0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110],
        'u' => [0, 0, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101],
        'v' => [0, 0, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'w' => [0, 0, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010],
        'x' => [0, 0, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001],
        'y' => [0, 0, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'z' => [0, 0, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110],
        '.' => [0, 0, 0, 0, 0, 0b00110, 0b00110],
        ',' => [0, 0, 0, 0, 0b00110, 0b00100, 0b01000],
        ';' => [0, 0, 0b00110, 0, 0b00110, 0b00100, 0b01000],
        ':' => [0, 0b00110, 0b00110, 0, 0b00110, 0b00110, 0],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        '?' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0, 0b00100],
        '\'' => [0b00100, 0b00100, 0b01000, 0, 0, 0, 0],
        '\u{2018}' | '\u{2019}' => [0b00100, 0b00100, 0b01000, 0, 0, 0, 0],
        '"' => [0b01010, 0b01010, 0b01010, 0, 0, 0, 0],
        '\u{201C}' | '\u{201D}' => [0b01010, 0b01010, 0b01010, 0, 0, 0, 0],
        '(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        ')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        '-' => [0, 0, 0, 0b01110, 0, 0, 0],
        '\u{2013}' | '\u{2014}' => [0, 0, 0, 0b01110, 0, 0, 0],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        '\\' => [0b10000, 0b01000, 0b01000, 0b00100, 0b00010, 0b00010, 0b00001],
        '@' => [0b01110, 0b10001, 0b10111, 0b10101, 0b10110, 0b10000, 0b01111],
        '#' => [0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010],
        '&' => [0b01100, 0b10010, 0b10100, 0b01010, 0b10101, 0b10010, 0b01101],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        '+' => [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
        '*' => [0, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0],
        '=' => [0, 0, 0b11111, 0, 0b11111, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 0b11111],
        '[' => [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110],
        ']' => [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110],
        '|' => [0b00100; 7],
        '~' => [0, 0, 0b01000, 0b10101, 0b00010, 0, 0],
        'ä' => [0b01010, 0, 0b01110, 0b00001, 0b01110, 0b10001, 0b01111],
        'ö' => [0b01010, 0, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
        'ü' => [0b01010, 0, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101],
        'ß' => [0b11110, 0b10001, 0b10001, 0b01110, 0b00010, 0b00010, 0b00010],
        'é' => [0b00010, 0b00100, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        _ => [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
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
