//! Firefox native messaging wire framing.
//!
//! Every message on the stdio channel is a little-endian `u32` byte length
//! followed by exactly that many bytes of UTF-8 JSON (Firefox / Chromium
//! native messaging specification, also documented in
//! `docs/extension-protocol.md` §1.2).
//!
//! The host writes nothing else to stdout: stdout is the protocol channel.
//! Logs go to `%LOCALAPPDATA%\RDM\logs\native.log` plus stderr (Firefox pipes
//! stderr into the browser console), never stdout.

use std::io::{self, Read, Write};

/// Hard cap for one inbound message.
///
/// Firefox allows up to 1 MiB *from the app to the browser*; messages *to* the
/// app may be larger. We apply the same 1 MiB ceiling inbound as a defensive
/// limit (an extension has no legitimate reason to send more).
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("native messaging i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("native message length {0} exceeds the {MAX_FRAME_BYTES} byte limit")]
    TooLarge(u32),
    #[error("truncated native message: stream ended after {0} bytes")]
    Truncated(usize),
}

/// Read one frame from `reader`.
///
/// * `Ok(Some(payload))` — a complete frame.
/// * `Ok(None)` — clean end of stream before any byte of a new frame.
/// * `Err(..)` — I/O error, truncated frame or an over-limit length prefix.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; 4];
    if !read_exact_or_clean_eof(reader, &mut header)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(header);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    read_exact(reader, &mut payload)?;
    Ok(Some(payload))
}

/// Write one frame: `u32_le(length)` + payload, then flush.
///
/// Firefox closes the pipe when the extension disconnects; a broken pipe here
/// is a normal shutdown path for the caller, not a protocol bug.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), FrameError> {
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// Returns `false` only when EOF happened before the first byte was read.
fn read_exact_or_clean_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<bool, FrameError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(FrameError::Truncated(filled));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(true)
}

fn read_exact<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Err(FrameError::Truncated(filled)),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encode(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for payload in payloads {
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn roundtrip_single_frame() {
        let payload = br#"{"type":"ping","id":7}"#;
        let encoded = encode(&[payload]);
        assert_eq!(&encoded[..4], &(payload.len() as u32).to_le_bytes());
        let mut cursor = Cursor::new(encoded);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(frame, payload);
        // Only then a clean EOF.
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn roundtrip_multiple_frames_via_writer() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"type":"ping"}"#).unwrap();
        write_frame(&mut buf, br#"{"type":"enqueue"}"#).unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cursor).unwrap().unwrap(),
            br#"{"type":"ping"}"#
        );
        assert_eq!(
            read_frame(&mut cursor).unwrap().unwrap(),
            br#"{"type":"enqueue"}"#
        );
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn clean_eof_on_empty_stream() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn truncated_header_is_an_error() {
        let mut cursor = Cursor::new(vec![0x05, 0x00]);
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, FrameError::Truncated(2)));
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let mut encoded = (10u32).to_le_bytes().to_vec();
        encoded.extend_from_slice(b"short");
        let mut cursor = Cursor::new(encoded);
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, FrameError::Truncated(5)));
    }

    #[test]
    fn oversized_length_prefix_is_rejected_before_reading_payload() {
        let mut cursor = Cursor::new((MAX_FRAME_BYTES + 1).to_le_bytes().to_vec());
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(n) if n == MAX_FRAME_BYTES + 1));
    }

    #[test]
    fn empty_payload_is_a_valid_frame() {
        // The JSON layer rejects it, but the framing layer must not confuse it
        // with EOF (Firefox itself never sends empty frames).
        let mut cursor = Cursor::new(0u32.to_le_bytes().to_vec());
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn write_frame_rejects_oversized_payload() {
        let mut out = Vec::new();
        let too_big = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        let err = write_frame(&mut out, &too_big).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
        assert!(
            out.is_empty(),
            "nothing must be written for a rejected frame"
        );
    }

    #[test]
    fn interrupted_reads_are_retried() {
        struct InterruptOnce {
            inner: Cursor<Vec<u8>>,
            interrupted: bool,
        }
        impl Read for InterruptOnce {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
                }
                self.inner.read(buf)
            }
        }
        let payload = b"{}";
        let encoded = encode(&[payload]);
        let mut reader = InterruptOnce {
            inner: Cursor::new(encoded),
            interrupted: false,
        };
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), payload);
    }
}
