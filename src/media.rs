//! Turn a photo or a short video into 720×720 JPEG frames for the pump.

use anyhow::{bail, Context, Result};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Square region of the source, in the source's own pixels.
/// `zoom` 1 keeps the largest square. Higher zoom crops tighter.
/// `pan_x` and `pan_y` are -1 (top/left) through 1 (bottom/right), 0 centered.
#[derive(Clone, Copy, Debug)]
pub struct Crop {
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl Crop {
    pub fn full() -> Self {
        Self {
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }

    /// `(side, x, y)` of an even square that stays inside the source.
    pub fn square(&self, width: u32, height: u32) -> (u32, u32, u32) {
        let zoom = self.zoom.clamp(1.0, 4.0);
        let mut side = ((width.min(height) as f32) / zoom).floor() as u32;
        side = (side.max(2)) & !1;
        if side > width {
            side = width & !1;
        }
        if side > height {
            side = height & !1;
        }
        side = side.max(2);
        let max_x = width.saturating_sub(side);
        let max_y = height.saturating_sub(side);
        let pan_x = self.pan_x.clamp(-1.0, 1.0);
        let pan_y = self.pan_y.clamp(-1.0, 1.0);
        let mut x = ((max_x as f32) * (pan_x + 1.0) / 2.0).round() as u32;
        let mut y = ((max_y as f32) * (pan_y + 1.0) / 2.0).round() as u32;
        x &= !1;
        y &= !1;
        if x > max_x {
            x = max_x;
        }
        if y > max_y {
            y = max_y;
        }
        (side, x, y)
    }

    pub fn ffmpeg_filter(&self, width: u32, height: u32) -> String {
        let (side, x, y) = self.square(width, height);
        format!("crop={side}:{side}:{x}:{y},scale=720:720")
    }
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub frames: Vec<Vec<u8>>,
    /// How long one frame lasts at 1× speed.
    pub frame_ms: u64,
}

pub fn photo_jpeg(path: &Path, crop: &Crop) -> Result<Vec<u8>> {
    let ffmpeg = require_ffmpeg()?;
    let probe = probe_media(&ffmpeg, path)?;
    let out =
        std::env::temp_dir().join(format!("strix-lc-screen-still-{}.jpg", std::process::id()));
    run_ffmpeg(
        &ffmpeg,
        &[
            "-y",
            "-i",
            &path.to_string_lossy(),
            "-an",
            "-vf",
            &crop.ffmpeg_filter(probe.width, probe.height),
            "-frames:v",
            "1",
            "-pix_fmt",
            "yuvj420p",
            "-q:v",
            "5",
            "-update",
            "1",
            &out.to_string_lossy(),
        ],
    )?;
    let bytes = std::fs::read(&out).with_context(|| format!("could not read {}", out.display()))?;
    let _ = std::fs::remove_file(&out);
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        bail!("ffmpeg did not produce a JPEG for {}", path.display());
    }
    Ok(bytes)
}

pub fn preview_jpeg(path: &Path) -> Result<Preview> {
    let ffmpeg = require_ffmpeg()?;
    let probe = probe_media(&ffmpeg, path)?;
    let out = std::env::temp_dir().join(format!(
        "strix-lc-screen-preview-{}.jpg",
        std::process::id()
    ));
    let mut args = vec!["-y".to_string()];
    if is_video(path) {
        args.extend(["-ss".to_string(), "0.2".to_string()]);
    }
    args.extend([
        "-i".to_string(),
        path.to_string_lossy().into_owned(),
        "-an".to_string(),
        "-vf".to_string(),
        "scale=480:480:force_original_aspect_ratio=decrease".to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-pix_fmt".to_string(),
        "yuvj420p".to_string(),
        "-q:v".to_string(),
        "6".to_string(),
        "-update".to_string(),
        "1".to_string(),
        out.to_string_lossy().into_owned(),
    ]);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ffmpeg(&ffmpeg, &arg_refs)?;
    let jpeg = std::fs::read(&out).with_context(|| format!("could not read {}", out.display()))?;
    let _ = std::fs::remove_file(&out);
    if jpeg.len() < 4 || jpeg[0] != 0xff || jpeg[1] != 0xd8 {
        bail!("ffmpeg did not produce a preview for {}", path.display());
    }
    Ok(Preview {
        width: probe.width,
        height: probe.height,
        jpeg,
    })
}

pub fn video_jpegs(path: &Path, crop: &Crop) -> Result<Clip> {
    let ffmpeg = require_ffmpeg()?;
    let probe = probe_media(&ffmpeg, path)?;
    let capture_fps = probe.fps.clamp(4.0, 8.0);
    let frame_ms = (1000.0 / capture_fps).round().max(1.0) as u64;
    let dir = std::env::temp_dir().join("strix-lc-screen-frames");
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir)?;
    let pattern = dir.join("frame_%04d.jpg");
    let fps = format!("{capture_fps:.3}");
    let filter = crop.ffmpeg_filter(probe.width, probe.height);
    let result = (|| {
        run_ffmpeg(
            &ffmpeg,
            &[
                "-y",
                "-i",
                &path.to_string_lossy(),
                "-an",
                "-t",
                "20",
                "-vf",
                &filter,
                "-r",
                &fps,
                "-pix_fmt",
                "yuvj420p",
                "-q:v",
                "5",
                &pattern.to_string_lossy(),
            ],
        )?;
        let mut frames = Vec::new();
        let mut index = 1u32;
        loop {
            let file = dir.join(format!("frame_{index:04}.jpg"));
            if !file.exists() {
                break;
            }
            frames.push(
                std::fs::read(&file)
                    .with_context(|| format!("could not read {}", file.display()))?,
            );
            index += 1;
            if frames.len() >= 160 {
                break;
            }
        }
        if frames.is_empty() {
            bail!("ffmpeg did not produce any frames from {}", path.display());
        }
        Ok(Clip { frames, frame_ms })
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

pub fn is_video(path: &Path) -> bool {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp4" | "mov" | "avi" | "mkv" | "webm" | "gif") => true,
        _ => false,
    }
}

fn require_ffmpeg() -> Result<PathBuf> {
    find_ffmpeg().context(
        "could not find ffmpeg. Release downloads include it next to this program. If you built from source, install ffmpeg or put it in the same folder",
    )
}

fn probe_media(ffmpeg: &Path, path: &Path) -> Result<Probe> {
    let mut command = Command::new(ffmpeg);
    hide_console(&mut command);
    let output = command
        .args(["-hide_banner", "-i"])
        .arg(path)
        .output()
        .with_context(|| format!("could not run {}", ffmpeg.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let (width, height, fps) = parse_probe(&text)
        .with_context(|| format!("could not read the size of {}", path.display()))?;
    Ok(Probe { width, height, fps })
}

fn parse_probe(text: &str) -> Option<(u32, u32, f32)> {
    for line in text.lines() {
        if !line.contains("Video:") {
            continue;
        }
        let (width, height) = find_size(line)?;
        let mut fps = 8.0;
        for part in line.split(',') {
            let part = part.trim();
            if let Some(num) = part.strip_suffix(" fps") {
                if let Ok(value) = num.trim().parse::<f32>() {
                    if value > 0.5 && value < 240.0 {
                        fps = value;
                    }
                }
            }
        }
        return Some((width, height, fps));
    }
    None
}

fn find_size(line: &str) -> Option<(u32, u32)> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index < bytes.len() && bytes[index] == b'x' {
                let width: u32 = line[start..index].parse().ok()?;
                index += 1;
                let height_start = index;
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
                if index > height_start {
                    let height: u32 = line[height_start..index].parse().ok()?;
                    if width >= 2 && height >= 2 {
                        return Some((width, height));
                    }
                }
            }
        } else {
            index += 1;
        }
    }
    None
}

fn run_ffmpeg(ffmpeg: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new(ffmpeg);
    hide_console(&mut command);
    let status = command
        .args(args)
        .status()
        .with_context(|| format!("could not run {}", ffmpeg.display()))?;
    if !status.success() {
        bail!("ffmpeg could not read the file");
    }
    Ok(())
}

fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        for candidate in ffmpeg_candidates(&exe) {
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    if let Some(path) = command_on_path("ffmpeg") {
        return Some(path);
    }
    #[cfg(windows)]
    {
        let asus = PathBuf::from(r"C:\Program Files\rog_strix_lc_iv\bin\ffmpeg.exe");
        if asus.is_file() {
            return Some(asus);
        }
    }
    None
}

fn ffmpeg_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

/// The copy shipped beside the program, then the copy the `.deb` installs under `/usr/lib`.
fn ffmpeg_candidates(exe: &Path) -> Vec<PathBuf> {
    let Some(dir) = exe.parent() else {
        return Vec::new();
    };
    let name = ffmpeg_name();
    vec![
        dir.join(name),
        dir.join("../lib/strix-lc-screen").join(name),
    ]
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

fn command_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_runnable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn is_runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_crop_uses_the_short_side() {
        let (side, x, y) = Crop::full().square(1280, 720);
        assert_eq!((side, x, y), (720, 280, 0));
        assert_eq!(
            Crop::full().ffmpeg_filter(1280, 720),
            "crop=720:720:280:0,scale=720:720"
        );
    }

    #[test]
    fn zoom_and_pan_stay_inside_the_frame() {
        let crop = Crop {
            zoom: 2.0,
            pan_x: -1.0,
            pan_y: 1.0,
        };
        let (side, x, y) = crop.square(1280, 720);
        assert_eq!(side, 360);
        assert_eq!(x, 0);
        assert_eq!(y, 360);
    }

    #[test]
    fn bundled_ffmpeg_is_beside_the_program_or_in_the_deb() {
        let candidates = ffmpeg_candidates(Path::new("/usr/bin/strix-lc-screen"));
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].file_name().and_then(|name| name.to_str()),
            Some(ffmpeg_name())
        );
        assert!(
            candidates[1].ends_with(Path::new("lib").join("strix-lc-screen").join(ffmpeg_name()))
        );
    }

    #[test]
    fn probe_line_reads_size_and_fps() {
        let text = "Stream #0:0: Video: h264, yuv420p, 1280x720 [SAR 1:1 DAR 16:9], 30 fps, 30 tbr";
        assert_eq!(parse_probe(text), Some((1280, 720, 30.0)));
    }
}
