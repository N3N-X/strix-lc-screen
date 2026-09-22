//! HID framing used by the ROG Strix LC IV pump screen.
//!
//! A logical frame is `5A | u16be length | payload | checksum | 5A`.
//! `length` counts every byte of the logical frame, including both markers.
//! The checksum is the low 8 bits of the sum of the length bytes and the payload.
//! On the wire, `5A` and `5B` inside the length, payload, and checksum are escaped
//! so a payload byte cannot look like the end marker. `5A` becomes `5B 01` and
//! `5B` becomes `5B 02`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Truncated,
    BadStart,
    BadLength,
    BadChecksum { expected: u8, actual: u8 },
    BadEscape,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Truncated => write!(f, "frame is incomplete"),
            FrameError::BadStart => write!(f, "frame does not start with 5A"),
            FrameError::BadLength => write!(f, "frame length does not match its header"),
            FrameError::BadChecksum { expected, actual } => {
                write!(f, "checksum was {actual:#04x}, expected {expected:#04x}")
            }
            FrameError::BadEscape => write!(f, "frame escape byte is incomplete"),
        }
    }
}

impl std::error::Error for FrameError {}

pub fn encode(payload: &[u8]) -> Vec<u8> {
    let logical_len = payload.len() + 5;
    let len_bytes = (logical_len as u16).to_be_bytes();
    let checksum = len_bytes
        .iter()
        .chain(payload.iter())
        .fold(0u8, |acc, byte| acc.wrapping_add(*byte));

    let mut interior = Vec::with_capacity(logical_len);
    interior.extend_from_slice(&len_bytes);
    interior.extend_from_slice(payload);
    interior.push(checksum);

    let mut wire = Vec::with_capacity(interior.len() + 2);
    wire.push(0x5a);
    for byte in interior {
        match byte {
            0x5a | 0x5b => {
                wire.push(0x5b);
                wire.push(byte - 0x59);
            }
            other => wire.push(other),
        }
    }
    wire.push(0x5a);
    wire
}

/// Decode one frame from the front of `input`.
/// Returns the frame and the number of wire bytes it occupied.
pub fn decode(input: &[u8]) -> Result<(Frame, usize), FrameError> {
    if input.is_empty() {
        return Err(FrameError::Truncated);
    }
    if input[0] != 0x5a {
        return Err(FrameError::BadStart);
    }

    let mut interior = Vec::new();
    let mut index = 1;
    let mut ended = false;
    while index < input.len() {
        let byte = input[index];
        if byte == 0x5b {
            if index + 1 >= input.len() {
                return Err(FrameError::BadEscape);
            }
            interior.push(input[index + 1].wrapping_add(0x59));
            index += 2;
            continue;
        }
        if byte == 0x5a {
            ended = true;
            index += 1;
            break;
        }
        interior.push(byte);
        index += 1;
    }
    if !ended {
        return Err(FrameError::Truncated);
    }
    if interior.len() < 3 {
        return Err(FrameError::BadLength);
    }

    let logical_len = u16::from_be_bytes([interior[0], interior[1]]) as usize;
    if logical_len != interior.len() + 2 {
        return Err(FrameError::BadLength);
    }
    let checksum = *interior.last().expect("length checked");
    let expected = interior[..interior.len() - 1]
        .iter()
        .fold(0u8, |acc, byte| acc.wrapping_add(*byte));
    if checksum != expected {
        return Err(FrameError::BadChecksum {
            expected,
            actual: checksum,
        });
    }
    let payload = interior[2..interior.len() - 1].to_vec();
    Ok((Frame { payload }, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(hex_str: &str) -> Vec<u8> {
        (0..hex_str.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
            .collect()
    }

    const CONN: &str = "5a0035504f535420636f6e6e20310d0a5365714e756d6265723d340d0a446174653d313738393933373633343838350d0a0d0a725a";
    const CONN_ESCAPED: &str = "5a0035504f535420636f6e6e20310d0a5365714e756d6265723d300d0a446174653d313739303030393936363533340d0a0d0a5b025a";
    const POWER: &str = "5a006c504f535420706f77657220310d0a5365714e756d6265723d310d0a446174653d313739303030393936363839320d0a436f6e74656e74547970653d6a736f6e0d0a436f6e74656e744c656e6774683d31380d0a0d0a7b226576656e74223a22726573756d65227d0b5a";

    #[test]
    fn logged_frames_roundtrip() {
        for hex_str in [CONN, CONN_ESCAPED, POWER] {
            let wire = sample(hex_str);
            let (frame, used) = decode(&wire).unwrap();
            assert_eq!(used, wire.len());
            assert_eq!(encode(&frame.payload), wire);
        }
    }

    #[test]
    fn payload_can_contain_the_end_marker() {
        let payload = b"sn=BYZL\x5a\x5b";
        let wire = encode(payload);
        assert!(wire.windows(2).any(|pair| pair == [0x5b, 0x01]));
        let (frame, used) = decode(&wire).unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(frame.payload, payload);
    }
}
