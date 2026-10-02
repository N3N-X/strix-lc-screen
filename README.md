# strix-lc-screen

A small app that drives the pump screen on a ROG Strix LC IV / SLC IV cooler. It replaces the official **ROG STRIX LC & SLC IV Series** program, which stays heavy while it is open.

The panel is a 720×720 screen on USB device `0B05:1DE7`. This app talks to that device directly. On Windows, quit the ASUS app first. Both programs want the same USB connection, and the official one wins if it is running.

## What you can do

- Set brightness, rotation (0°, 90°, 180°, 270°), and wake or sleep the screen.
- Show a photo, or play a short video, with a crop box and a zoom slider.
- Change playback speed from 0.25× to 2× while the clip is running. 1× matches the length of the video.
- Close the window and leave the clip running. A tray icon stays behind.
- Start the app when you sign in, hidden in the tray if you want, and send the last file again on its own.

Click the tray icon to open the window. Right-click it to stop the video or quit. **Stop video** leaves the last frame on the pump. **Quit** exits the program.

## The pump does not keep the clip

Custom pictures are sent as JPEG frames from the PC. The cooler does not store your video. Playback continues while this program is running, including when the window is hidden in the tray. After you quit, the pump goes back to its built-in clip.

Video is limited to about 20 seconds, sampled at up to 8 frames per second.

## Requirements

- Windows or Linux
- [Rust](https://rustup.rs/) if you are building it yourself
- `ffmpeg` on `PATH`. On Windows, the copy that ships with the ASUS app also works:

  `C:\Program Files\rog_strix_lc_iv\bin\ffmpeg.exe`

## Build and run

```powershell
cargo build --release
.\target\release\strix-lc-screen.exe
```

With no arguments, that opens the window.

Command-line controls, for when you do not want the window:

```powershell
.\target\release\strix-lc-screen.exe status
.\target\release\strix-lc-screen.exe brightness 80
.\target\release\strix-lc-screen.exe rotate 180
.\target\release\strix-lc-screen.exe power resume
.\target\release\strix-lc-screen.exe power suspend
.\target\release\strix-lc-screen.exe show "C:\Pictures\photo.png"
```

`power resume` wakes the screen. `power suspend` puts it to sleep. Rotation is 0, 90, 180, or 270. Brightness is 0 through 100.

Sign-in startup uses `--tray`, which opens straight into the tray.

Settings are saved in `%APPDATA%\strix-lc-screen\settings.json` on Windows, and in `~/.config/strix-lc-screen/settings.json` on Linux.

## Linux

The screen is still USB `0B05:1DE7`. Linux opens it through hidraw, so the logged-in user needs permission. Copy the rule, reload udev, then unplug the cooler and plug it back in:

```bash
sudo cp linux/60-strix-lc-screen.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Build dependencies on Debian and Ubuntu:

```bash
sudo apt install build-essential pkg-config libudev-dev libxkbcommon-dev \
  libwayland-dev libx11-dev libxcb1-dev libxcursor-dev libxi-dev libxrandr-dev \
  libgl1-mesa-dev ffmpeg
```

```bash
cargo build --release
./target/release/strix-lc-screen
```

The same commands as on Windows work here (`status`, `brightness`, `rotate`, `power`, `show`). The tray icon is a StatusNotifier item, which GNOME, KDE, and Sway already show. Closing the window leaves the clip running. A second launch brings the window back.

Sign-in startup writes `~/.config/autostart/strix-lc-screen.desktop`. **Choose file** uses the desktop portal, so `xdg-desktop-portal` has to be installed. The ASUS program does not run on Linux, so there is nothing else to quit before opening the screen.

## License

MIT. See [LICENSE](LICENSE).
