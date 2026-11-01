This project helps you read ebooks faster

## Usage

```sh
cargo run                          # read dummy.epub (or: cargo run <book.epub>)
cargo run -- --delay 0.2           # faster: start at one word every 0.2 s
cargo run -- --summary             # print a chapter/word summary instead
cargo run -- --text                # print the summary plus the full text
```

By default the program shows the book's words in the terminal one at a time
(RSVP, rapid-serial visual presentation), starting with the word changing every
second (set a different start pace with `--delay`). Each word is rendered in a
smooth anti-aliased serif (a bundled DejaVu Serif) and balanced around a fixed
vertical center line: a vowel near the middle of the word is highlighted in red
as the recognition point and placed on the center line, while the rest of the
word is dimmed by its distance from it. The pace in words/minute is shown in
the bottom-right corner; the speed and progress (`[ n/total ]`) are on the
status line at the top.

Keys while the display is running:

| key        | effect                                        |
|------------|-----------------------------------------------|
| `+` / `=`  | speed up by 5 words/s                         |
| `-`        | slow down by 5 words/s                        |
| `space`    | pause / resume                                |
| `p`        | pause and show the last 30 words plus the current word in the normal font; press again to resume |
| `q` / Esc  | quit (`Ctrl+C` also works)                    |

Pressing `p` pauses the display and shows the current word together with the
last 30 words in the normal terminal font (fewer words are shown near the start
of the book); pressing `p` or `space` again resumes.
