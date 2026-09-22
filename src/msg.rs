//! Text messages carried inside a frame payload.

use anyhow::{anyhow, bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        Ok(serde_json::from_slice(&self.body)?)
    }
}

pub fn request(cmd: &str, seq: u32, date_ms: u128, body: Option<&[u8]>) -> Vec<u8> {
    request_typed(cmd, seq, date_ms, "json", body)
}

pub fn request_typed(
    cmd: &str,
    seq: u32,
    date_ms: u128,
    content_type: &str,
    body: Option<&[u8]>,
) -> Vec<u8> {
    request_parts(cmd, seq, date_ms, &[], content_type, body)
}

pub fn request_parts(
    cmd: &str,
    seq: u32,
    date_ms: u128,
    extra_headers: &[(&str, String)],
    content_type: &str,
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut text = format!("POST {cmd} 1\r\nSeqNumber={seq}\r\nDate={date_ms}\r\n");
    if let Some(body) = body {
        if !content_type.is_empty() {
            text.push_str(&format!("ContentType={content_type}\r\n"));
            text.push_str(&format!("ContentLength={}\r\n", body.len()));
        }
        for (name, value) in extra_headers {
            text.push_str(name);
            text.push('=');
            text.push_str(value);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    } else {
        for (name, value) in extra_headers {
            text.push_str(name);
            text.push('=');
            text.push_str(value);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        text.into_bytes()
    }
}

pub fn parse_response(payload: &[u8]) -> Result<Response> {
    let text_end = payload
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("response has no header terminator"))?;
    let head = std::str::from_utf8(&payload[..text_end])
        .map_err(|_| anyhow!("response header is not text"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .ok_or_else(|| anyhow!("response is empty"))?;
    let mut parts = status.split_whitespace();
    let version = parts
        .next()
        .ok_or_else(|| anyhow!("response status line is empty"))?;
    if version != "1" {
        bail!("unexpected response version {version}");
    }
    let code: u16 = parts
        .next()
        .ok_or_else(|| anyhow!("response has no status code"))?
        .parse()
        .map_err(|_| anyhow!("status code is not a number"))?;

    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("bad response header {line}"))?;
        headers.push((name.to_string(), value.to_string()));
    }
    let body = payload[text_end + 4..].to_vec();
    if let Some(length) = headers
        .iter()
        .find(|(name, _)| name == "ContentLength")
        .and_then(|(_, value)| value.parse::<usize>().ok())
    {
        if length != body.len() {
            bail!("ContentLength was {length}, body is {} bytes", body.len());
        }
    }
    Ok(Response {
        code,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode, encode};

    #[test]
    fn rebuilds_the_logged_connect_request() {
        let payload = request("conn", 4, 1_789_937_634_885, None);
        let wire = encode(&payload);
        let expected = hex("5a0035504f535420636f6e6e20310d0a5365714e756d6265723d340d0a446174653d313738393933373633343838350d0a0d0a725a");
        assert_eq!(wire, expected);
    }

    #[test]
    fn rebuilds_the_logged_power_request() {
        let payload = request("power", 1, 1_790_009_966_892, Some(br#"{"event":"resume"}"#));
        assert_eq!(encode(&payload), hex("5a006c504f535420706f77657220310d0a5365714e756d6265723d310d0a446174653d313739303030393936363839320d0a436f6e74656e74547970653d6a736f6e0d0a436f6e74656e744c656e6774683d31380d0a0d0a7b226576656e74223a22726573756d65227d0b5a"));
    }

    #[test]
    fn parses_a_logged_state_response() {
        let wire = hex("5a00f231203230300d0a41636b4e756d6265723d330d0a436f6e74656e74547970653d6a736f6e0d0a436f6e74656e744c656e6774683d3137380d0a0d0a7b227370616365223a36353839362c226272696768746e657373223a3130302c22646567726565223a3138302c226f73645374617465223a302c226d6f6465223a302c226c6f676f223a332c2274696d656f7574223a352c22626f6f7446696e697368223a312c22646973706c6179496e536c656570223a312c227072657365745468656d654964223a302c22736c656570436c6f636b4964223a302c227761746572426c6f636b53637265656e223a317ddc5a");
        let (frame, _) = decode(&wire).unwrap();
        let response = parse_response(&frame.payload).unwrap();
        assert_eq!(response.code, 200);
        assert_eq!(response.header("AckNumber"), Some("3"));
        let json = response.json().unwrap();
        assert_eq!(json["brightness"], 100);
        assert_eq!(json["degree"], 180);
        assert_eq!(json["space"], 65896);
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }
}
