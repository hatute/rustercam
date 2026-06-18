use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

use crate::{recording::TcamDecoder, ui::TerminalGuard};

pub fn run_playback(path: &Path) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut decoder = TcamDecoder::open(path)?;
    let mut stdout = io::stdout();
    let start = Instant::now();
    let mut frame_num = 0usize;
    loop {
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                    break;
                }
            }
        }
        let Some((frame, timestamp_ms)) = decoder.read_frame()? else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        write!(stdout, "\x1b[H")?;
        for (y, row) in frame.chars.iter().enumerate() {
            if let Some(colors) = &frame.colors {
                for (x, ch) in row.chars().enumerate() {
                    let (r, g, b) = colors
                        .get(y)
                        .and_then(|r| r.get(x))
                        .copied()
                        .unwrap_or((255, 255, 255));
                    write!(stdout, "\x1b[38;2;{r};{g};{b}m{ch}")?;
                }
                writeln!(stdout, "\x1b[0m\x1b[K")?;
            } else {
                writeln!(stdout, "{row}\x1b[K")?;
            }
        }
        writeln!(
            stdout,
            "\x1b[7m RUSTERCAM PLAYBACK | frame {frame_num} | {}x{} | {} fps | q quit \x1b[0m\x1b[K",
            decoder.width, decoder.height, decoder.fps
        )?;
        stdout.flush()?;
        frame_num += 1;
        let target = Duration::from_millis(timestamp_ms as u64);
        if target > start.elapsed() {
            std::thread::sleep(target - start.elapsed());
        }
    }
    Ok(())
}
