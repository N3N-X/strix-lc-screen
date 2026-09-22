#![cfg_attr(windows, windows_subsystem = "windows")]

mod frame;
mod gui;
mod media;
mod msg;
mod session;
mod settings;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use session::Panel;

#[derive(Parser)]
#[command(
    name = "strix-lc-screen",
    about = "Control the ROG Strix LC IV pump screen without the ASUS app"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read firmware, brightness, rotation, and free space.
    Status,
    /// Set backlight brightness from 0 to 100.
    Brightness { value: u8 },
    /// Set the picture rotation in degrees. The panel stores 0, 90, 180, or 270.
    Rotate { degrees: u16 },
    /// Wake the panel or put it to sleep. The setting is stored on the device.
    Power { event: PowerEvent },
    /// Send one photo to the pump. The picture stays until the next one.
    Show { path: std::path::PathBuf },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum PowerEvent {
    Resume,
    Suspend,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tray_only = args.len() == 1 && args[0] == "--tray";
    if args.is_empty() || tray_only {
        if let Err(error) = gui::run(tray_only) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    attach_console();
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return Ok(());
    };
    let mut panel = Panel::open()?;
    let identity = panel
        .connect()
        .context("the screen did not accept the handshake")?;
    match command {
        Command::Status => {
            let state = panel.state()?;
            print_state(&identity, &state);
        }
        Command::Brightness { value } => {
            ensure_official_app_is_closed()?;
            if value > 100 {
                bail!("brightness must be from 0 to 100");
            }
            panel.brightness(value)?;
            let state = panel.state()?;
            println!("brightness is {}", state["brightness"]);
        }
        Command::Rotate { degrees } => {
            ensure_official_app_is_closed()?;
            panel.rotate(degrees)?;
            let state = panel.state()?;
            println!("rotation is {} degrees", state["degree"]);
        }
        Command::Power { event } => {
            ensure_official_app_is_closed()?;
            let name = match event {
                PowerEvent::Resume => "resume",
                PowerEvent::Suspend => "suspend",
            };
            panel.power(name)?;
            println!("power {name} sent");
        }
        Command::Show { path } => {
            ensure_official_app_is_closed()?;
            let jpeg = media::photo_jpeg(&path, &media::Crop::full())?;
            println!("sending {} bytes", jpeg.len());
            panel.show_jpeg(&jpeg, &|| false)?;
            println!("picture sent");
        }
    }
    Ok(())
}

fn attach_console() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
            fn AllocConsole() -> i32;
        }
        if AttachConsole(0xFFFF_FFFF) == 0 {
            AllocConsole();
        }
    }
}

fn ensure_official_app_is_closed() -> Result<()> {
    let mut command = std::process::Command::new("tasklist");
    command.args(["/FO", "CSV", "/NH"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .output()
        .context("could not check whether the ASUS app is running")?;
    let listing = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if listing.contains("rog strix lc") {
        bail!(
            "Quit \"ROG STRIX LC & SLC IV Series\" first. It uses the same USB connection, so the two programs would mix up each other's replies."
        );
    }
    Ok(())
}

fn print_state(identity: &serde_json::Value, state: &serde_json::Value) {
    let version = &identity["version"];
    println!(
        "serial:     {}",
        identity["sn"].as_str().unwrap_or("unknown")
    );
    println!(
        "firmware:   {}",
        version["firmware"].as_str().unwrap_or("unknown")
    );
    println!(
        "hardware:   {}",
        version["hardware"].as_str().unwrap_or("unknown")
    );
    println!("brightness: {}", state["brightness"]);
    println!("rotation:   {}", state["degree"]);
    println!("free space: {}", state["space"]);
    println!("sleep show: {}", state["displayInSleep"]);
}
