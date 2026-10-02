Windows and Linux builds. ffmpeg is included.

A small app for the ROG Strix LC IV / SLC IV pump screen (USB 0B05:1DE7). It talks to the panel directly. On Windows, quit "ROG STRIX LC & SLC IV Series" before you open it. Both programs want the same USB connection.

- strix-lc-screen-windows-x86_64.zip. Unzip it and run strix-lc-screen.exe. Keep ffmpeg.exe in that folder.
- strix-lc-screen-linux-x86_64.tar.gz. The program, ffmpeg, and a udev rule.
- strix-lc-screen_0.1.1_amd64.deb for Debian and Ubuntu.

The package installs the program and the USB permission. After it is installed, unplug the cooler and plug it back in, then run strix-lc-screen. The tarball has the same program, plus ffmpeg and the udev rule, if you would rather not use the package. Copy that rule, reload udev, and unplug the cooler once.

What you can do:

- Set brightness, rotation (0°, 90°, 180°, 270°), and wake or sleep the screen.
- Show a photo or a short video, with a crop box and a zoom slider.
- Change playback speed from 0.25× to 2× while the clip is running. 1× matches the length of the video.
- Close the window and leave the clip running from the tray icon.
- Start when you sign in, stay in the tray, and send the last file again on its own.

Click the tray icon to open the window. Right-click it to stop the video or quit. Stop video leaves the last frame on the pump. Quit exits the program.

The cooler does not store your clip. Pictures are sent as JPEG frames from this computer, and playback continues while this program is running, including from the tray. After you quit, the pump goes back to its built-in clip. Video is limited to about 20 seconds, sampled at up to 8 frames per second.

Settings are saved in %APPDATA%\strix-lc-screen\settings.json on Windows, and in ~/.config/strix-lc-screen/settings.json on Linux.
