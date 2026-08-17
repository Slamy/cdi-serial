//! CD-i Stub protocol implementation.
//!
//! The wire format is documented in `cdistub-0.5.1/stub/stubdefs.d` from the
//! CD-i Stub distribution.  Multi-byte numbers are big-endian and every frame
//! ends in the XOR of all preceding frame bytes (including its header).

use std::fmt;
use std::io::{self, Read, Write};

pub const SOH: u8 = 0x01;
pub const DLE: u8 = 0x10;
pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;
// The Stub protocol uses 0x14 for CAN (rather than ASCII CAN, 0x18).
pub const CAN: u8 = 0x14;
pub const EM: u8 = 0x19;

pub const WRITE: u8 = 0x01;
pub const ADDRESS: u8 = 0x02;
pub const EXECUTE: u8 = 0x04;
pub const END: u8 = 0x08;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Cancelled,
    UnexpectedNotification(u8),
    RetryLimitExceeded {
        operation: &'static str,
        attempts: u8,
    },
    InvalidChunkSize(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "serial I/O error: {err}"),
            Self::Cancelled => write!(f, "CD-i cancelled the operation"),
            Self::UnexpectedNotification(byte) => {
                write!(f, "unexpected protocol notification 0x{byte:02X}")
            }
            Self::RetryLimitExceeded {
                operation,
                attempts,
            } => {
                write!(f, "{operation} was rejected {attempts} times")
            }
            Self::InvalidChunkSize(size) => write!(f, "chunk size {size} must be in 1..=65535"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Builds a host request. `body` excludes the SOH and message-type bytes.
pub fn request(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 3);
    frame.extend([SOH, kind]);
    frame.extend_from_slice(body);
    let check = frame.iter().fold(0, |check, byte| check ^ byte);
    frame.push(check);
    frame
}

/// A host-side session over any byte stream, so the protocol can be tested
/// without a physical player.
pub struct Session<T> {
    io: T,
    retries: u8,
}

impl<T: Read + Write> Session<T> {
    pub fn new(io: T) -> Self {
        Self { io, retries: 3 }
    }

    pub fn with_retries(io: T, retries: u8) -> Self {
        Self { io, retries }
    }

    pub fn into_inner(self) -> T {
        self.io
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.io
    }

    /// Waits for a positive acknowledgement, resending a request after NAK.
    fn send_acknowledged(&mut self, frame: &[u8], operation: &'static str) -> Result<()> {
        for _ in 0..=self.retries {
            self.io.write_all(&frame[..1])?;
            self.io.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(1));
            self.io.write_all(&frame[1..])?;
            self.io.flush()?;
            match self.read_acknowledgement()? {
                ACK => return Ok(()),
                NAK => continue,
                CAN => return Err(Error::Cancelled),
                _ => unreachable!("read_acknowledgement only returns notifications"),
            }
        }
        Err(Error::RetryLimitExceeded {
            operation,
            attempts: self.retries + 1,
        })
    }

    fn read_notification(&mut self) -> Result<u8> {
        let mut byte = [0];
        self.io.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    /// Reads an ACK/NAK/CAN response. Some USB serial adapters and null-modem
    /// wiring echo transmitted bytes; CD-i Link also has to resynchronise past
    /// startup text. Ignore those bytes until the actual one-byte response.
    fn read_acknowledgement(&mut self) -> Result<u8> {
        for _ in 0..4096 {
            match self.read_notification()? {
                notification @ (ACK | NAK | CAN) => return Ok(notification),
                _ => continue,
            }
        }
        Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "received more than 4096 non-notification bytes while waiting for acknowledgement",
        )))
    }

    pub fn set_address(&mut self, address: u32) -> Result<()> {
        self.send_acknowledged(&request(ADDRESS, &address.to_be_bytes()), "address request")
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > u16::MAX as usize {
            return Err(Error::InvalidChunkSize(data.len()));
        }
        let mut body = Vec::with_capacity(data.len() + 2);
        body.extend_from_slice(&(data.len() as u16).to_be_bytes());
        body.extend_from_slice(data);
        self.send_acknowledged(&request(WRITE, &body), "write request")
    }

    pub fn upload(&mut self, address: u32, data: &[u8], chunk_size: usize) -> Result<()> {
        if !(1..=u16::MAX as usize).contains(&chunk_size) {
            return Err(Error::InvalidChunkSize(chunk_size));
        }
        self.set_address(address)?;
        for chunk in data.chunks(chunk_size) {
            self.write(chunk)?;
        }
        Ok(())
    }

    /// Starts code at `address`. The target first returns ACK; a returning
    /// stub application will later send EM.
    pub fn execute(&mut self, address: u32) -> Result<()> {
        self.send_acknowledged(&request(EXECUTE, &address.to_be_bytes()), "execute request")
    }

    pub fn wait_for_execution_end(&mut self) -> Result<()> {
        match self.read_notification()? {
            EM => Ok(()),
            CAN => Err(Error::Cancelled),
            byte => Err(Error::UnexpectedNotification(byte)),
        }
    }

    pub fn end(&mut self) -> Result<()> {
        self.send_acknowledged(&request(END, &[]), "end request")
    }

    /// Reads startup text until a CD-i endpoint announces itself.
    ///
    /// A full Stub prints a banner then sends `EM`. The player ROM's download
    /// subset instead starts its bootstrap with `SOH`; CD-i Link must answer
    /// that byte with `ACK` before changing to its transfer baud rate.
    pub fn wait_for_stub(&mut self, max_bytes: usize) -> Result<Vec<u8>> {
        let mut banner = Vec::new();
        while banner.len() < max_bytes {
            let byte = self.read_notification()?;
            if byte == SOH {
                self.io.write_all(&[ACK])?;
                self.io.flush()?;
                return Ok(banner);
            }
            if byte == EM || byte == DLE {
                return Ok(banner);
            }
            banner.push(byte);
        }
        Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "stub activation banner exceeds limit",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestIo {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl TestIo {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: std::io::Cursor::new(input),
                output: Vec::new(),
            }
        }
    }

    impl Read for TestIo {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for TestIo {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frame_is_big_endian_and_xor_checked() {
        assert_eq!(
            request(ADDRESS, &[0x12, 0x34, 0x56, 0x78]),
            vec![SOH, ADDRESS, 0x12, 0x34, 0x56, 0x78, 0x0B]
        );
    }

    #[test]
    fn nak_retries_the_identical_frame() {
        let stream = TestIo::new(vec![NAK, ACK]);
        let mut session = Session::with_retries(stream, 1);
        session.set_address(0x1000).unwrap();
        let stream = session.into_inner();
        let frame = request(ADDRESS, &0x1000_u32.to_be_bytes());
        assert_eq!(stream.output, [frame.clone(), frame].concat());
    }

    #[test]
    fn echo_before_acknowledgement_is_ignored() {
        let stream = TestIo::new(vec![SOH, ADDRESS, 0x00, ACK]);
        let mut session = Session::new(stream);
        session.set_address(0x8000).unwrap();
    }

    #[test]
    fn rom_download_subset_announces_with_dle() {
        let stream = TestIo::new(vec![DLE]);
        let mut session = Session::new(stream);
        assert!(session.wait_for_stub(16).unwrap().is_empty());
    }

    #[test]
    fn rom_download_subset_soh_is_acknowledged() {
        let stream = TestIo::new(vec![SOH]);
        let mut session = Session::new(stream);
        session.wait_for_stub(16).unwrap();
        assert_eq!(session.into_inner().output, vec![ACK]);
    }

    #[test]
    fn upload_splits_writes_and_advances_on_target() {
        let stream = TestIo::new(vec![ACK, ACK, ACK]);
        let mut session = Session::new(stream);
        session.upload(0x2000, &[1, 2, 3, 4, 5], 3).unwrap();
        let bytes = session.into_inner();
        let expected = [
            request(ADDRESS, &0x2000_u32.to_be_bytes()),
            request(WRITE, &[0, 3, 1, 2, 3]),
            request(WRITE, &[0, 2, 4, 5]),
        ]
        .concat();
        assert_eq!(bytes.output, expected);
    }
}
