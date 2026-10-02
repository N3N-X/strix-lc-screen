# strix-lc-screen

This drives the pump screen on a ROG Strix LC IV / SLC IV cooler, so you can quit the official **ROG STRIX LC & SLC IV Series** app. That program stays heavy the whole time it is open.

The panel is a 720×720 screen on USB device `0B05:1DE7`. This program talks to it directly. On Windows, quit the ASUS app first. Both want the same USB connection, and the official one wins if it is still running.

You can set brightness, turn the picture (0°, 90°, 180°, 270°), and wake or sleep the screen. You can show a photo or a short video, drag a crop box, and zoom. Speed runs from 0.25× to 2× while the clip is playing. 1× is the real length of the video.

Close the window and the clip keeps going. A tray icon stays. Click it to open the window again. Right-click it for **Stop video** or **Quit**. Stop leaves the last frame on the pump. Quit exits, and the pump goes back to its built-in clip.

You can start it when you sign in, straight into the tray if you want, and send the last file again on its own.

The cooler does not store your video. Pictures are JPEG frames sent from this computer, about 20 seconds long, at up to 8 frames a second. Playback lasts only while this program is running, including when the window is hidden.

## Downloads

Publish a release on GitHub and the build attaches three files to it:

- `strix-lc-screen-windows-x86_64.zip`, the Windows program and ffmpeg
- `strix-lc-screen-linux-x86_64.tar.gz`, the Linux program, ffmpeg, and the udev rule
- `strix-lc-screen_<version>_amd64.deb`, for Debian and Ubuntu, ffmpeg included

You do not install ffmpeg yourself. Keep it in the same folder as the program. The `.deb` puts it in the right place for you. After `sudo apt install ./strix-lc-screen_*_amd64.deb`, unplug the cooler and plug it back in once, so the new USB permission applies. Then run `strix-lc-screen`.

If you use the tarball instead of the package, copy the rule yourself:

```bash
sudo cp 60-strix-lc-screen.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Unplug and plug the cooler back in after that too.

The tray shows on KDE, on Sway, and on Ubuntu. Plain GNOME hides it until you install the AppIndicator extension. **Choose file** goes through the desktop portal, so install `xdg-desktop-portal` if that button does nothing. The ASUS program is Windows-only, so on Linux there is nothing else to close first.

## Build it yourself

Install [Rust](https://rustup.rs/), then:

```powershell
cargo build --release
.\target\release\strix-lc-screen.exe
```

On Debian or Ubuntu the compile also needs:

```bash
sudo apt install build-essential pkg-config libudev-dev libxkbcommon-dev \
  libwayland-dev libx11-dev libxcb1-dev libxcursor-dev libxi-dev libxrandr-dev \
  libgl1-mesa-dev
cargo build --release
./target/release/strix-lc-screen
```

A source build does not contain ffmpeg. Install it, or put the `ffmpeg` binary in the same folder as the program. To build the `.deb` with ffmpeg inside, run `./packaging/fetch-ffmpeg.sh`, then `cargo install cargo-deb` and `cargo deb`. The package lands in `target/debian/`.

No arguments opens the window. `--tray` opens straight into the tray, which is what sign-in startup uses.

```powershell
.\target\release\strix-lc-screen.exe status
.\target\release\strix-lc-screen.exe brightness 80
.\target\release\strix-lc-screen.exe rotate 180
.\target\release\strix-lc-screen.exe power resume
.\target\release\strix-lc-screen.exe power suspend
.\target\release\strix-lc-screen.exe show "C:\Pictures\photo.png"
```

The same words work on Linux, with `./target/release/strix-lc-screen` in front. `power resume` wakes the screen. `power suspend` puts it to sleep. Rotation is 0, 90, 180, or 270. Brightness is 0 through 100.

Settings are saved in `%APPDATA%\strix-lc-screen\settings.json` on Windows, and in `~/.config/strix-lc-screen/settings.json` on Linux. Sign-in on Windows is a registry entry. On Linux it is `~/.config/autostart/strix-lc-screen.desktop`.

## License

MIT. See [LICENSE](LICENSE).
