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
pub const READ: u8 = 0x81;
pub const BAUDRATE: u8 = 0x80;
/// Allocate a temporary buffer in the full Stub.
pub const BUFFER: u8 = 0x82;
/// Select the full Stub's saved 68000 registers as the current data area.
pub const REGISTERS: u8 = 0x83;
/// Invoke an OS-9 system call through the full Stub.
pub const OS9CALL: u8 = 0x84;

/// Mask bit for the 68000 D0 register in a full Stub register request.
pub const REG_D0: u32 = 1 << 0;
/// Mask bit for the 68000 D1 register in a full Stub register request.
pub const REG_D1: u32 = 1 << 1;
/// Mask bit for the 68000 A0 register in a full Stub register request.
pub const REG_A0: u32 = 1 << 8;
/// Set in an OS-9-call response when the OS-9 call failed. D1 contains the
/// OS-9 error code when this bit is present in the returned mask.
pub const REG_CARRY: u32 = 1 << 31;

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
    InvalidReadResponse,
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
            Self::InvalidReadResponse => write!(f, "invalid response to read request"),
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
    raw_uart_trace: bool,
    tx_pacing: Option<TxPacing>,
}

#[derive(Clone, Copy)]
struct TxPacing {
    physical_baud: u32,
    effective_baud: u32,
}

impl<T: Read + Write> Session<T> {
    pub fn new(io: T) -> Self {
        Self {
            io,
            retries: 3,
            raw_uart_trace: false,
            tx_pacing: None,
        }
    }

    pub fn with_retries(io: T, retries: u8) -> Self {
        Self {
            io,
            retries,
            raw_uart_trace: false,
            tx_pacing: None,
        }
    }

    /// Enables a hexadecimal trace of every byte transferred through the
    /// session. This is intended for diagnosing serial-link and Stub protocol
    /// problems, and is written to standard error.
    pub fn set_raw_uart_trace(&mut self, enabled: bool) {
        self.raw_uart_trace = enabled;
    }

    /// Adds a delay after each UART transmission so a physically faster link
    /// behaves like `effective_baud`. This does not change the serial port's
    /// configured baud rate.
    pub fn set_tx_pacing(&mut self, physical_baud: u32, effective_baud: u32) {
        self.tx_pacing =
            (physical_baud > effective_baud && effective_baud > 0).then_some(TxPacing {
                physical_baud,
                effective_baud,
            });
    }

    /// Disables transmit pacing while retaining all other session settings.
    pub fn clear_tx_pacing(&mut self) {
        self.tx_pacing = None;
    }

    pub fn into_inner(self) -> T {
        self.io
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.io
    }

    fn trace_uart(&self, direction: &str, bytes: &[u8]) {
        if self.raw_uart_trace && !bytes.is_empty() {
            eprint!("UART {direction}:");
            for byte in bytes {
                eprint!(" {byte:02X}");
            }
            eprintln!();
        }
    }

    fn pace_tx_byte(&self) {
        let Some(pacing) = self.tx_pacing else {
            return;
        };
        // 8N1 UART framing uses ten bits per byte. The physical transfer is
        // already paced at `physical_baud`; wait only for the additional time
        // needed to approximate the requested effective baud rate.
        let bits = 10_u128;
        let target_ns = bits * 1_000_000_000 / u128::from(pacing.effective_baud);
        let physical_ns = bits * 1_000_000_000 / u128::from(pacing.physical_baud);
        let extra_ns = target_ns.saturating_sub(physical_ns);
        if extra_ns > 0 {
            std::thread::sleep(std::time::Duration::from_nanos(
                extra_ns.min(u128::from(u64::MAX)) as u64,
            ));
        }
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            // USB serial writes are ordinarily buffered, so delaying after a
            // complete frame does not reliably space its individual UART
            // characters. MiSTer mode therefore submits one byte at a time.
            let write_buffer = if self.tx_pacing.is_some() {
                &bytes[..1]
            } else {
                bytes
            };
            match self.io.write(write_buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write complete UART frame",
                    ));
                }
                Ok(written) => {
                    self.trace_uart("TX", &bytes[..written]);
                    bytes = &bytes[written..];
                    self.pace_tx_byte();
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn read_exact(&mut self, mut bytes: &mut [u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match self.io.read(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "UART closed while reading a frame",
                    ));
                }
                Ok(read) => {
                    self.trace_uart("RX", &bytes[..read]);
                    let (_, remaining) = bytes.split_at_mut(read);
                    bytes = remaining;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }

    /// Waits for a positive acknowledgement, resending a request after NAK.
    fn send_acknowledged(&mut self, frame: &[u8], operation: &'static str) -> Result<()> {
        for _ in 0..=self.retries {
            self.write_all(&frame[..1])?;
            self.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(1));
            self.write_all(&frame[1..])?;
            self.flush()?;
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
        self.read_exact(&mut byte)?;
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

    /// Finds a full-Stub response header while tolerating echoed request
    /// bytes or terminal text that was already queued on the serial link.
    fn read_response_header(&mut self, expected_kind: u8) -> Result<()> {
        for _ in 0..4096 {
            match self.read_notification()? {
                CAN => return Err(Error::Cancelled),
                DLE if self.read_notification()? == expected_kind => return Ok(()),
                _ => continue,
            }
        }
        Err(Error::InvalidReadResponse)
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

    /// Allocates a temporary full-Stub buffer and makes it the current data
    /// area. Returns its actual size and target address.
    pub fn allocate_buffer(&mut self, size: usize) -> Result<(usize, u32)> {
        if !(1..=u16::MAX as usize).contains(&size) {
            return Err(Error::InvalidChunkSize(size));
        }
        self.send_acknowledged(
            &request(BUFFER, &(size as u16).to_be_bytes()),
            "buffer request",
        )?;
        self.read_response_header(BUFFER)?;
        let mut response = [0; 6];
        self.read_exact(&mut response)?;
        let checksum = self.read_notification()?;
        let computed = [DLE, BUFFER]
            .into_iter()
            .chain(response)
            .fold(0, |check, byte| check ^ byte);
        if checksum != computed {
            self.write_all(&[NAK])?;
            self.flush()?;
            return Err(Error::InvalidReadResponse);
        }
        self.write_all(&[ACK])?;
        self.flush()?;
        Ok((
            usize::from(u16::from_be_bytes([response[0], response[1]])),
            u32::from_be_bytes([response[2], response[3], response[4], response[5]]),
        ))
    }

    /// Selects saved 68000 registers and writes their big-endian values in
    /// ascending register-bit order. The full Stub accepts 32-bit values for
    /// the Dn/An registers used by the public constants above.
    pub fn set_registers(&mut self, mask: u32, values: &[u32]) -> Result<()> {
        if values.len() != mask.count_ones() as usize {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "register value count does not match register mask",
            )));
        }
        self.select_registers(mask)?;
        let mut data = Vec::with_capacity(values.len() * 4);
        for value in values {
            data.extend_from_slice(&value.to_be_bytes());
        }
        self.write(&data)
    }

    /// Selects saved 68000 registers as the current data area without
    /// modifying them. A subsequent [`read`](Self::read) retrieves their
    /// values in ascending register-bit order.
    pub fn select_registers(&mut self, mask: u32) -> Result<()> {
        self.send_acknowledged(&request(REGISTERS, &mask.to_be_bytes()), "register request")
    }

    /// Invokes an OS-9 call through a full Stub. `result_mask` asks the Stub
    /// to preserve selected registers for the caller; the returned mask also
    /// reports registers changed by OS-9 and the carry/error state.
    pub fn os9_call(&mut self, call: u16, result_mask: u32) -> Result<u32> {
        let mut body = Vec::with_capacity(6);
        body.extend_from_slice(&call.to_be_bytes());
        body.extend_from_slice(&result_mask.to_be_bytes());
        self.send_acknowledged(&request(OS9CALL, &body), "OS-9 call request")?;
        self.read_response_header(OS9CALL)?;
        let mut mask = [0; 4];
        self.read_exact(&mut mask)?;
        let checksum = self.read_notification()?;
        let computed = [DLE, OS9CALL]
            .into_iter()
            .chain(mask)
            .fold(0, |check, byte| check ^ byte);
        if checksum != computed {
            self.write_all(&[NAK])?;
            self.flush()?;
            return Err(Error::InvalidReadResponse);
        }
        self.write_all(&[ACK])?;
        self.flush()?;
        Ok(u32::from_be_bytes(mask))
    }

    /// Downloads host data to CD-i memory.
    pub fn download(&mut self, address: u32, data: &[u8], chunk_size: usize) -> Result<()> {
        self.download_with_progress(address, data, chunk_size, |_| {})
    }

    /// Downloads host data and reports the cumulative acknowledged byte count after
    /// each successful WRITE request.
    pub fn download_with_progress<F>(
        &mut self,
        address: u32,
        data: &[u8],
        chunk_size: usize,
        mut progress: F,
    ) -> Result<()>
    where
        F: FnMut(usize),
    {
        if !(1..=u16::MAX as usize).contains(&chunk_size) {
            return Err(Error::InvalidChunkSize(chunk_size));
        }
        self.set_address(address)?;
        let mut transferred = 0;
        for chunk in data.chunks(chunk_size) {
            self.write(chunk)?;
            transferred += chunk.len();
            progress(transferred);
        }
        Ok(())
    }

    /// Reads up to 65535 bytes from the current address. This request is only
    /// supported by a full `cdi_stub`, not the player's ROM download subset.
    pub fn read(&mut self, size: usize) -> Result<Vec<u8>> {
        if !(1..=u16::MAX as usize).contains(&size) {
            return Err(Error::InvalidChunkSize(size));
        }
        self.send_acknowledged(&request(READ, &(size as u16).to_be_bytes()), "read request")?;

        for _ in 0..=self.retries {
            let marker = self.read_notification()?;
            if marker == CAN {
                return Err(Error::Cancelled);
            }
            if marker != DLE || self.read_notification()? != READ {
                return Err(Error::InvalidReadResponse);
            }
            let mut size_bytes = [0; 2];
            self.read_exact(&mut size_bytes)?;
            if usize::from(u16::from_be_bytes(size_bytes)) != size {
                return Err(Error::InvalidReadResponse);
            }
            let mut data = vec![0; size];
            self.read_exact(&mut data)?;
            let checksum = self.read_notification()?;
            let computed = [DLE, READ]
                .into_iter()
                .chain(size_bytes)
                .chain(data.iter().copied())
                .fold(0, |check, byte| check ^ byte);
            if checksum == computed {
                self.write_all(&[ACK])?;
                self.flush()?;
                return Ok(data);
            }
            self.write_all(&[NAK])?;
            self.flush()?;
        }
        Err(Error::RetryLimitExceeded {
            operation: "read response",
            attempts: self.retries + 1,
        })
    }

    /// Uploads `size` bytes starting at `address`, reporting the cumulative
    /// byte count after each acknowledged response.
    pub fn upload_with_progress<F>(
        &mut self,
        address: u32,
        size: usize,
        chunk_size: usize,
        mut progress: F,
    ) -> Result<Vec<u8>>
    where
        F: FnMut(usize),
    {
        if !(1..=u16::MAX as usize).contains(&chunk_size) {
            return Err(Error::InvalidChunkSize(chunk_size));
        }
        self.set_address(address)?;
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let count = (size - data.len()).min(chunk_size);
            data.extend(self.read(count)?);
            progress(data.len());
        }
        Ok(data)
    }

    /// Asks a full Stub for the highest supported baud rate not exceeding
    /// `desired`. The caller must switch its local serial port to the returned
    /// non-zero rate immediately after this succeeds.
    pub fn negotiate_baud_rate(&mut self, desired: u32) -> Result<u32> {
        self.send_acknowledged(
            &request(BAUDRATE, &desired.to_be_bytes()),
            "baud-rate request",
        )?;
        let marker = self.read_notification()?;
        if marker == CAN {
            return Err(Error::Cancelled);
        }
        if marker != DLE || self.read_notification()? != BAUDRATE {
            return Err(Error::InvalidReadResponse);
        }
        let mut rate = [0; 4];
        self.read_exact(&mut rate)?;
        let checksum = self.read_notification()?;
        let computed = [DLE, BAUDRATE]
            .into_iter()
            .chain(rate)
            .fold(0, |check, byte| check ^ byte);
        if checksum != computed {
            self.write_all(&[NAK])?;
            self.flush()?;
            return Err(Error::RetryLimitExceeded {
                operation: "baud-rate response",
                attempts: 1,
            });
        }
        self.write_all(&[ACK])?;
        self.flush()?;
        Ok(u32::from_be_bytes(rate))
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
                self.write_all(&[ACK])?;
                self.flush()?;
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
        writes: Vec<Vec<u8>>,
    }

    impl TestIo {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: std::io::Cursor::new(input),
                output: Vec::new(),
                writes: Vec::new(),
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
            self.writes.push(buffer.to_vec());
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
    fn tx_pacing_submits_each_uart_byte_separately() {
        let stream = TestIo::new(vec![ACK]);
        let mut session = Session::new(stream);
        session.set_tx_pacing(115_200, 38_000);
        session.set_address(0x1000).unwrap();
        let stream = session.into_inner();
        assert!(stream.writes.iter().all(|write| write.len() == 1));
        assert_eq!(
            stream.writes.len(),
            request(ADDRESS, &0x1000_u32.to_be_bytes()).len()
        );
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
    fn download_splits_writes_and_advances_on_target() {
        let stream = TestIo::new(vec![ACK, ACK, ACK]);
        let mut session = Session::new(stream);
        session.download(0x2000, &[1, 2, 3, 4, 5], 3).unwrap();
        let bytes = session.into_inner();
        let expected = [
            request(ADDRESS, &0x2000_u32.to_be_bytes()),
            request(WRITE, &[0, 3, 1, 2, 3]),
            request(WRITE, &[0, 2, 4, 5]),
        ]
        .concat();
        assert_eq!(bytes.output, expected);
    }

    #[test]
    fn download_reports_only_acknowledged_chunks() {
        let stream = TestIo::new(vec![ACK, ACK, ACK]);
        let mut session = Session::new(stream);
        let mut progress = Vec::new();
        session
            .download_with_progress(0x2000, &[1, 2, 3, 4, 5], 3, |bytes| progress.push(bytes))
            .unwrap();
        assert_eq!(progress, vec![3, 5]);
    }

    #[test]
    fn read_validates_response_and_acknowledges_it() {
        let data = [0x12, 0x34, 0x56];
        let mut response = vec![ACK, DLE, READ, 0, data.len() as u8];
        response.extend(data);
        response.push(response.iter().skip(1).fold(0, |check, byte| check ^ byte));
        let stream = TestIo::new(response);
        let mut session = Session::new(stream);
        assert_eq!(session.read(3).unwrap(), data);
        let stream = session.into_inner();
        assert_eq!(stream.output, [request(READ, &[0, 3]), vec![ACK]].concat());
    }

    #[test]
    fn upload_sets_address_and_reports_progress() {
        let mut first_response = vec![DLE, READ, 0, 3, 1, 2, 3];
        first_response.push(first_response.iter().fold(0, |check, byte| check ^ byte));
        let mut second_response = vec![DLE, READ, 0, 2, 4, 5];
        second_response.push(second_response.iter().fold(0, |check, byte| check ^ byte));
        let stream =
            TestIo::new([vec![ACK, ACK], first_response, vec![ACK], second_response].concat());
        let mut session = Session::new(stream);
        let mut progress = Vec::new();
        assert_eq!(
            session
                .upload_with_progress(0x2000, 5, 3, |bytes| progress.push(bytes))
                .unwrap(),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(progress, vec![3, 5]);
    }

    #[test]
    fn baud_rate_is_negotiated_before_local_port_switch() {
        let rate = 38_400_u32.to_be_bytes();
        let mut response = vec![ACK, DLE, BAUDRATE];
        response.extend(rate);
        response.push(response.iter().skip(1).fold(0, |check, byte| check ^ byte));
        let stream = TestIo::new(response);
        let mut session = Session::new(stream);
        assert_eq!(session.negotiate_baud_rate(38_400).unwrap(), 38_400);
        assert_eq!(
            session.into_inner().output,
            [request(BAUDRATE, &rate), vec![ACK]].concat()
        );
    }

    #[test]
    fn full_stub_buffer_response_is_validated_and_acknowledged() {
        let mut response = vec![ACK, DLE, BUFFER, 0, 0x40, 0, 0xdf, 0xbe, 0x10];
        response.push(response.iter().skip(1).fold(0, |check, byte| check ^ byte));
        let stream = TestIo::new(response);
        let mut session = Session::new(stream);
        assert_eq!(session.allocate_buffer(64).unwrap(), (64, 0x00df_be10));
        assert_eq!(
            session.into_inner().output,
            [request(BUFFER, &[0, 0x40]), vec![ACK]].concat()
        );
    }

    #[test]
    fn full_stub_os9_call_returns_changed_register_mask() {
        let mask = (REG_D0 | REG_CARRY).to_be_bytes();
        let mut response = vec![ACK, DLE, OS9CALL];
        response.extend(mask);
        response.push(response.iter().skip(1).fold(0, |check, byte| check ^ byte));
        let stream = TestIo::new(response);
        let mut session = Session::new(stream);
        assert_eq!(session.os9_call(0x84, 0).unwrap(), REG_D0 | REG_CARRY);
        assert_eq!(
            session.into_inner().output,
            [request(OS9CALL, &[0, 0x84, 0, 0, 0, 0]), vec![ACK]].concat()
        );
    }
}
