This project helps you read ebooks faster

## Usage

```sh
cargo run                          # summary of dummy.epub (or: cargo run <book.epub>)
cargo run -- --text                # print the full text
cargo run -- --read                # display the words one after the other,
                                   # changing the word every 1 second (Ctrl+C to stop)
cargo run -- --read --delay 0.2    # faster: one word every 0.2 s
```

The `--read` mode (RSVP, rapid-serial visual presentation) shows the book's
words in the terminal one at a time, starting with the word changing every
second (set a different start pace with `--delay`).

Keys while the display is running:

| key        | effect                    |
|------------|---------------------------|
| `+` / `=`  | speed up by 5 words/s     |
| `-`        | slow down by 5 words/s    |
| `space`    | pause / resume            |
| `q` / Esc  | quit (`Ctrl+C` also works) |

Each word is rendered as giant block letters (a built-in 5x7 font stretched
to fill the whole terminal), so every word fills the screen. The speed and
progress (`[ n/total ]`) are shown on the status line at the top.
