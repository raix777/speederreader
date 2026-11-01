# AGENTS.md

Rules for AI agents working in this repo. Not aimed at humans; be precise, not
polished.

## What this is

Rust CLI that loads an EPUB (a ZIP with a small, well-defined XML layout) and
shows its reading structure. By default it runs a rapid-serial display (RSVP):
the book's words appear in the terminal one after the other, each drawn in a
smooth anti-aliased serif (a bundled DejaVu Serif, `assets/dejavuserif.ttf`,
rasterized with `rusttype`) and balanced around a fixed vertical center line —
a vowel near the middle of the word is highlighted in red as the recognition
point (ORP) and placed on the line, the rest dimmed by distance. A
chapter/word summary is available with `--summary`, and the full text with
`--text`. Pure Rust, no hardware; every behavior is unit-testable without an
EPUB.

## Layout — keep the three layers separate

Everything lives in one file, `src/main.rs`, in three layers. Keep code in the
layer where its concern already lives; do not let the top layers reach into the
bottom ones.

- **EPUB parsing** — `Epub`/`Chapter`, `Epub::load`, and the "EPUB internals"
  (`read_entry`, `opf_path_from_container`, `parse_opf`, `parent_dir`,
  `join_paths`). Walks the archive: `mimetype` → `container.xml` → OPF
  (metadata + manifest + spine) → each spine item's XHTML. Uses `zip` +
  `roxmltree`. Knows nothing about the terminal.
- **Text extraction** — `extract_text`, `heading_from`, `first_text`,
  `render_into`/`push_render`, `is_block`, `normalize`, `strip_tags`,
  `count_words`. Turns one XHTML document into plain text, in document order.
  Pure (string in, string out).
- **Display** — `rsvp_display`, `display_loop`, `status_line`, `term_size`,
  `build_frame`, `visible_len`, `write_frame`, `draw_frame`,
  `draw_review_frame`, `wrap_text`, and the word rendering: ORP selection
  (`orp_index`, `letter_bright`), the embedded-font rasterization
  (`load_font`, `word_advance`, `fit_em`, `rasterize_word`), and the pure
  layout/color (`downsample`, `char_at`, `word_left_offset`, `word_frame`,
  `half_block`, `paint_*`, `row_to_line`). Talks to the terminal only through
  `crossterm`.

Rules:
- Keep the word-geometry and frame-assembly logic pure (no terminal I/O) so it
  stays testable: `orp_index`, `letter_bright`, `downsample`, `char_at`,
  `word_left_offset`, `word_frame`, `half_block`, `wrap_text`, `build_frame`,
  and `visible_len` take plain values and return data. The embedded-font
  functions (`load_font`, `word_advance`, `fit_em`, `rasterize_word`) are
  deterministic but load the bundled font. Only `rsvp_display`, `display_loop`,
  `term_size`, and `write_frame` touch the terminal. All frame output goes
  through `write_frame` → `build_frame`.
- Errors are `BoxedError` (`Box<dyn Error>`); load failures print to stderr and
  `exit(1)`.

## Build / test / run

- `cargo build --release`
- `cargo test` — no EPUB or hardware needed; the in-module tests cover the pure
  functions (`orp_index`, `downsample`, `word_frame`, `build_frame`, ...) and
  the embedded-font path (it ships in the repo, so it works offline).
- `./target/release/speederreader [book.epub]` (defaults to `dummy.epub`).
  Flags: `--summary`/`-s` (chapter/word summary), `--text`/`-t` (full text),
  `--delay <s>` (starting pace), `--read`/`-r` (display — already the default).
  While displaying: `+`/`=` speed up, `-` slow down, `space` pause/resume, `p`
  review (last 30 words), `q`/`Esc`/`Ctrl+C` quit.

## How to work (process)

- TDD. Write the failing test first, then the smallest code that passes it.
  Every behavior lives in the `#[cfg(test)] mod tests` next to its code, with a
  descriptive test name (e.g. `a_very_long_word_shrinks_to_fit_instead_of_overflowing`).
- Work in small steps: the least steps necessary to reach the goal. No
  speculative features; no refactoring beyond what the task requires.
- Clean code, self-explanatory: clear naming, small functions, straightforward
  structure. A reader should not need a comment to know what a line does.
- Descriptive names for types, functions, fields, variables, and constants
  (`Epub`, `parse_opf`, `orp_index`, `SPEED_STEP`, `MIN_SPEED`).
- Separation of concerns: keep every working unit as small as possible; one unit
  does one thing.
- Comments: avoid them. Add one only when it explains **why** — a decision,
  workaround, constraint, or non-obvious behavior (e.g. "a bare \n keeps the
  cursor column, so each frame line is \r\n-prefixed"). Never restate **what**
  the code does. Keep necessary comments concise, and update them when the
  related code changes.

## Style (match the existing code)

- Rust, edition 2021. Deps are lean (`zip`, `roxmltree`, `crossterm`,
  `rusttype`); add a dependency only when it clearly earns its place. The word
  font is a bundled TTF (`assets/dejavuserif.ttf`) embedded via `include_bytes!`,
  so the display does not depend on the host's installed fonts.
- Constants are `UPPER_SNAKE_CASE` (`SPEED_STEP`, `MIN_SPEED`, `TRAIL_WORDS`).
- Prefer `Option`/`Result` and early returns over deep nesting; use
  `saturating_*` arithmetic for the terminal geometry so nothing underflows on a
  tiny terminal.
- Terminal drawing is full-screen: hide the cursor and disable autowrap, redraw
  the whole screen with one buffered `write_frame` per frame (no flicker), place
  every line at column 0 and pad it to the width (no residue from a longer
  previous frame), and emit no feed after the last line (no scroll). Do not
  reintroduce the bare-`\n` line break that drifts the cursor off column 0.
