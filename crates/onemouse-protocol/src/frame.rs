//! Framing: each message is a little-endian `u32` byte length followed by the
//! `postcard` encoding of a [`Message`].

use std::fmt;
use std::io::{self, Read, Write};

use crate::Message;

/// Largest accepted frame body. Anything bigger is a protocol error.
pub const MAX_FRAME_LEN: usize = 1 << 20;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge(usize),
    Decode(postcard::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::TooLarge(len) => write!(f, "frame of {len} bytes exceeds {MAX_FRAME_LEN}"),
            Self::Decode(e) => write!(f, "malformed message: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::TooLarge(_) => None,
            Self::Decode(e) => Some(e),
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Encodes `msg` as a complete frame (length prefix included).
pub fn encode(msg: &Message) -> Result<Vec<u8>, FrameError> {
    let mut buf = vec![0; 4];
    buf = postcard::to_extend(msg, buf).map_err(FrameError::Decode)?;
    let len = buf.len() - 4;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    buf[..4].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(buf)
}

/// Decodes a frame body (without the length prefix).
pub fn decode(body: &[u8]) -> Result<Message, FrameError> {
    postcard::from_bytes(body).map_err(FrameError::Decode)
}

pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> Result<(), FrameError> {
    w.write_all(&encode(msg)?)?;
    Ok(())
}

/// Reads one frame. Returns `FrameError::Io` with `UnexpectedEof` when the peer
/// closed the connection.
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, FrameError> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    decode(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    fn all_messages() -> Vec<Message> {
        let display = Display {
            id: 1,
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.25,
            primary: true,
        };
        vec![
            Message::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                name: "desktop".into(),
                os: Os::Windows,
                displays: vec![display.clone()],
            }),
            Message::Welcome {
                protocol_version: PROTOCOL_VERSION,
                name: "macbook".into(),
                os: Os::MacOs,
                displays: vec![display.clone()],
                main: Main::Client,
            },
            Message::Reject {
                reason: "version mismatch".into(),
            },
            Message::DisplaysChanged {
                displays: vec![display],
            },
            Message::Enter { x: 10, y: -20 },
            Message::Leave,
            Message::MouseMove { x: 1919, y: 1079 },
            Message::MouseButton {
                button: MouseButton::Forward,
                pressed: true,
            },
            Message::Scroll { dx: -3, dy: 240 },
            Message::Key {
                code: key::LEFT_CTRL,
                pressed: false,
            },
            Message::Ping(u64::MAX),
            Message::Pong(0),
            Message::SetMain { main: Main::Server },
            Message::Arrangement { x: 1440, y: -120 },
        ]
    }

    #[test]
    fn round_trips_every_message_over_a_stream() {
        let messages = all_messages();
        let mut stream = Vec::new();
        for msg in &messages {
            write_message(&mut stream, msg).unwrap();
        }
        let mut r = stream.as_slice();
        for msg in &messages {
            assert_eq!(&read_message(&mut r).unwrap(), msg);
        }
        let eof = read_message(&mut r).unwrap_err();
        assert!(matches!(eof, FrameError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof));
    }

    /// Pins the wire layout. If this fails you changed the encoding: bump
    /// `PROTOCOL_VERSION` and get the other side's sign-off (see crate docs).
    #[test]
    fn wire_layout_is_stable() {
        let move_frame = encode(&Message::MouseMove { x: 1, y: -1 }).unwrap();
        assert_eq!(move_frame, [3, 0, 0, 0, 6, 2, 1]);

        let key_frame = encode(&Message::Key {
            code: key::A,
            pressed: true,
        })
        .unwrap();
        assert_eq!(key_frame, [3, 0, 0, 0, 9, 4, 1]);

        let hello = encode(&Message::Hello(Hello {
            protocol_version: 1,
            name: "w".into(),
            os: Os::Windows,
            displays: vec![],
        }))
        .unwrap();
        assert_eq!(hello, [6, 0, 0, 0, 0, 1, 1, b'w', 1, 0]);
    }

    /// An older peer reads a newer `Welcome` far enough to see the version
    /// and refuse cleanly: the fields it knows come first, unchanged.
    #[test]
    fn welcome_starts_with_version_and_name() {
        let frame = encode(&Message::Welcome {
            protocol_version: 3,
            name: "m".into(),
            os: Os::MacOs,
            displays: vec![],
            main: Main::Server,
        })
        .unwrap();
        assert_eq!(frame[4..], [1, 3, 1, b'm', 0, 0, 0]);
        assert_eq!(
            encode(&Message::SetMain { main: Main::Client }).unwrap(),
            [2, 0, 0, 0, 12, 1]
        );
    }

    #[test]
    fn rejects_oversized_frames_before_allocating() {
        let mut r: &[u8] = &(MAX_FRAME_LEN as u32 + 1).to_le_bytes();
        assert!(matches!(read_message(&mut r), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(decode(&[200]), Err(FrameError::Decode(_))));
    }
}
