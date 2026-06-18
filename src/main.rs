mod app;
mod capture;
mod cli;
mod export;
mod frame;
mod playback;
mod recording;
mod render;
mod ui;

use anyhow::Result;
use clap::Parser;

use app::App;
use cli::Args;
use playback::run_playback;

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(path) = args.play.clone() {
        return run_playback(&path);
    }
    App::new(args).run()
}
