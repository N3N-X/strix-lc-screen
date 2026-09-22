//! One HID session with the pump screen.

use crate::frame::{self, FrameError};
use crate::msg::{self, Response};
use anyhow::{anyhow, bail, Context, Result};
use hidapi::{DeviceInfo, HidApi, HidDevice};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VENDOR: u16 = 0x0b05;
const PRODUCT: u16 = 0x1de7;
const USAGE_PAGE: u16 = 0xff00;
const USAGE: u16 = 0x0001;

pub struct Panel {
    device: HidDevice,
    seq: u32,
    /// Reused 1025-byte picture report. Filling this beats allocating one per slice.
    report: Vec<u8>,
}

impl Panel {
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("HID library failed to start")?;
        let info = find_screen(&api)?;
        let device = info.open_device(&api).context(
            "could not open the pump screen. Quit \"ROG STRIX LC & SLC IV Series\" and try again",
        )?;
        device
            .set_blocking_mode(false)
            .context("could not set the screen read mode")?;
        Ok(Self {
            device,
            seq: 0,
            report: vec![0u8; 1025],
        })
    }

    pub fn connect(&mut self) -> Result<serde_json::Value> {
        let response = self.call("conn", None)?;
        expect_ok(&response, "conn")?;
        response.json().context("connect response was not JSON")
    }

    pub fn state(&mut self) -> Result<serde_json::Value> {
        let response = self.call("devStateGet", None)?;
        expect_ok(&response, "devStateGet")?;
        response.json().context("state response was not JSON")
    }

    pub fn power(&mut self, event: &str) -> Result<()> {
        let body = serde_json::json!({ "event": event }).to_string();
        let response = self.call("power", Some(body.as_bytes()))?;
        expect_ok(&response, "power")
    }

    pub fn brightness(&mut self, value: u8) -> Result<()> {
        let body = serde_json::json!({ "value": value }).to_string();
        let response = self.call("brightness", Some(body.as_bytes()))?;
        expect_ok(&response, "brightness")
    }

    pub fn rotate(&mut self, degrees: u16) -> Result<()> {
        let body = serde_json::json!({ "degree": degrees }).to_string();
        let response = self.call("rotate", Some(body.as_bytes()))?;
        expect_ok(&response, "rotate")
    }

    /// Wake the panel, turn on the live picture channel, and push one JPEG.
    /// The cooler shows that picture until the next one arrives.
    pub fn show_jpeg(&mut self, jpeg: &[u8], cancel: &dyn Fn() -> bool) -> Result<()> {
        if jpeg.is_empty() {
            bail!("picture is empty");
        }
        if cancel() {
            return Ok(());
        }
        let _ = self.power("resume");
        let enable = br#"{"enable":true}"#;
        let response = self.call("realtimeDisplay", Some(enable))?;
        if response.code != 200 {
            let body = String::from_utf8_lossy(&response.body);
            bail!("the screen refused the picture channel: {body}");
        }
        self.transport_jpeg(jpeg, cancel)
    }

    /// Push the next video frame without repeating the wake and channel setup.
    pub fn push_jpeg(&mut self, jpeg: &[u8], cancel: &dyn Fn() -> bool) -> Result<()> {
        self.transport_jpeg(jpeg, cancel)
    }

    fn transport_jpeg(&mut self, jpeg: &[u8], cancel: &dyn Fn() -> bool) -> Result<()> {
        // One 1025-byte report per slice: report id 0x00, tag 0x5C, big-endian
        // value length, a 21-byte header, then up to 1000 JPEG bytes.
        // Header: id, block count, zero-based index, flag 1, then 15 zero bytes.
        const CHUNK: usize = 1000;
        let blocks = jpeg.len().div_ceil(CHUNK).max(1) as u16;
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| (elapsed.as_secs() & 0xff) as u8)
            .unwrap_or(1);
        let mut offset = 0usize;
        let mut index = 0u16;
        while offset < jpeg.len() {
            if cancel() {
                return Ok(());
            }
            let end = (offset + CHUNK).min(jpeg.len());
            let chunk = &jpeg[offset..end];
            if self.report.len() != 1025 {
                self.report.resize(1025, 0);
            }
            self.report.fill(0);
            let value_len = (21 + chunk.len()) as u16;
            self.report[0] = 0x00;
            self.report[1] = 0x5c;
            self.report[2..4].copy_from_slice(&value_len.to_be_bytes());
            self.report[4] = id;
            self.report[5..7].copy_from_slice(&blocks.to_be_bytes());
            self.report[7..9].copy_from_slice(&index.to_be_bytes());
            self.report[9] = 0x01;
            self.report[25..25 + chunk.len()].copy_from_slice(chunk);
            let wrote = self
                .device
                .write(&self.report)
                .context("could not write the picture to the pump")?;
            if wrote == 0 {
                bail!("the pump did not accept the picture");
            }
            offset = end;
            index = index.saturating_add(1);
        }
        Ok(())
    }

    fn call(&mut self, cmd: &str, body: Option<&[u8]>) -> Result<Response> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let date = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the unix epoch")?
            .as_millis();
        let payload = msg::request(cmd, seq, date, body);
        self.exchange(cmd, seq, &payload)
    }

    fn exchange(&mut self, cmd: &str, seq: u32, payload: &[u8]) -> Result<Response> {
        let wire = frame::encode(payload);
        let mut report = Vec::with_capacity(wire.len() + 1);
        report.push(0x00);
        report.extend_from_slice(&wire);
        let wrote = self
            .device
            .write(&report)
            .with_context(|| format!("HID write failed for {cmd}"))?;
        if wrote < report.len() {
            bail!("{cmd} wrote {wrote} of {} bytes", report.len());
        }

        let expected_ack = seq.wrapping_add(1).to_string();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut pending = Vec::new();
        while Instant::now() < deadline {
            let mut buf = vec![0u8; 64 * 1024];
            let read = self
                .device
                .read_timeout(&mut buf, 200)
                .with_context(|| format!("HID read failed for {cmd}"))?;
            if read == 0 {
                continue;
            }
            pending.extend_from_slice(&buf[..read]);
            if pending.first() == Some(&0x00) && pending.get(1) == Some(&0x5a) {
                pending.remove(0);
            }
            match frame::decode(&pending) {
                Ok((frame, used)) => {
                    pending.drain(..used);
                    let response = msg::parse_response(&frame.payload)
                        .with_context(|| format!("{cmd} response was not a text message"))?;
                    if response.header("AckNumber") != Some(expected_ack.as_str()) {
                        continue;
                    }
                    return Ok(response);
                }
                Err(FrameError::Truncated | FrameError::BadEscape) => continue,
                Err(error) => return Err(error).context(format!("{cmd} returned a bad frame")),
            }
        }
        bail!("{cmd} got no matching reply within 2 seconds")
    }
}

fn find_screen(api: &HidApi) -> Result<DeviceInfo> {
    let mut found = Vec::new();
    for device in api.device_list() {
        let vendor_ok = device.vendor_id() == VENDOR;
        let product_ok = device.product_id() == PRODUCT;
        let usage_ok = device.usage_page() == USAGE_PAGE && device.usage() == USAGE;
        if vendor_ok && product_ok && usage_ok {
            found.push(device.clone());
        }
    }
    found.sort_by_key(|device| device.interface_number());
    found
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no ROG Strix LC IV screen found (USB 0B05:1DE7 interface 0)"))
}

fn expect_ok(response: &Response, cmd: &str) -> Result<()> {
    if response.code == 200 {
        return Ok(());
    }
    let body = String::from_utf8_lossy(&response.body);
    bail!("{cmd} was rejected with status {}: {body}", response.code)
}
