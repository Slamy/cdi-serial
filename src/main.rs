use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    io::{Read, Write},
    sync::Mutex,
    time::Duration,
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use cdi_serial::{REG_A0, REG_CARRY, REG_D0, REG_D1, Session};
use clap::{Parser, Subcommand};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};

const MISTER_BAUD: u32 = 115_200;

#[derive(Debug, Parser)]
#[command(
    name = "cdi-serial",
    version,
    about = "Download applications and upload memory over the CD-i Stub serial protocol"
)]
struct Cli {
    /// Serial device, e.g. /dev/ttyUSB0 on Linux or COM3 on Windows.
    #[arg(long)]
    port: String,
    /// Initial serial speed. CD-i Link waits at 9600, then the ROM download
    /// subset switches to 19200 after its bootstrap acknowledgement.
    #[arg(long, default_value_t = 9600)]
    baud: u32,
    /// Read timeout in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
    /// MiSTer serial mode: force the host connection to 115200 baud and never
    /// perform a protocol-directed local baud-rate switch.
    #[arg(long)]
    mister: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Wait until a running CD-i Stub announces itself.
    Wait {
        #[arg(long, default_value_t = 4096)]
        max_banner_bytes: usize,
    },
    /// Download an application image from the host to CD-i memory, optionally
    /// start it, then end the stub.
    Download {
        /// Image to transfer.
        file: String,
        /// Target memory address. CD-i Link addresses are hexadecimal; `8000`,
        /// `0x8000`, and `$8000` all mean 0x8000.
        #[arg(long, value_parser = parse_address)]
        address: u32,
        /// Maximum bytes per WRITE request (1 through 65535).
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
        /// First wait for the stub/download-subset startup marker (EM or DLE).
        /// This is implied by --reset.
        #[arg(long)]
        wait: bool,
        /// Send Ctrl-C (0x03) before the transfer, matching the common CD-i
        /// development workflow for stopping a previously loaded application.
        #[arg(long)]
        reset: bool,
        /// Send EXECUTE after transfer.
        #[arg(long)]
        execute: bool,
        /// Wait for the executed routine to return (EM notification).
        #[arg(long, requires = "execute")]
        wait_for_return: bool,
        /// Send END after download. On the built-in download subset this resumes boot.
        #[arg(long)]
        end: bool,
        /// Keep the serial connection open after the download and print CD-i
        /// debug output. Exit with Ctrl-C.
        #[arg(long)]
        terminal: bool,
        /// Serial speed to use after download, immediately before entering
        /// --terminal. Defaults to the current transfer speed.
        #[arg(long, requires = "terminal")]
        terminal_baud: Option<u32>,
        /// Append incoming terminal bytes to this file as well as printing
        /// them to standard output. Requires --terminal.
        #[arg(long, value_name = "FILE", requires = "terminal")]
        terminal_log: Option<String>,
    },
    /// Download an OS-9 full Stub module, then start it through the ROM
    /// download subset.
    Stub {
        /// Path to the cdistub OS-9 `play` module.
        file: String,
        /// Target memory address (hexadecimal). The standard cdistub module
        /// uses 0x8000.
        #[arg(long, default_value = "8000", value_parser = parse_address)]
        address: u32,
        /// Maximum bytes per WRITE request (1 through 65535).
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
    },
    /// List entries in an OS-9 directory through a running full CD-i Stub.
    Dir {
        /// OS-9 directory path, for example /nvr.
        path: String,
        /// Wait for the full Stub activation marker before listing.
        #[arg(long)]
        wait: bool,
        /// Bytes requested from OS-9 per directory read. This must be a
        /// multiple of the 32-byte OS-9 directory-entry size.
        #[arg(long, default_value_t = 256)]
        read_size: usize,
    },
    /// Copy an OS-9 file from the CD-i player to the host through a running
    /// full CD-i Stub.
    Get {
        /// Source OS-9 path on the CD-i player, for example /cd/copyright.
        remote_path: String,
        /// Destination file on the host. It is written after a successful
        /// complete transfer only.
        local_file: String,
        /// Bytes requested from OS-9 per read (1 through 65535).
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
        /// Wait for the full Stub activation marker before transferring.
        #[arg(long)]
        wait: bool,
    },
    /// Copy a host file to an OS-9 path on the CD-i player through a running
    /// full CD-i Stub.
    Put {
        /// Source file on the host.
        local_file: String,
        /// Destination OS-9 path on the CD-i player, for example
        /// /nvr/settings.bin. The path must not already exist.
        remote_path: String,
        /// Bytes written to OS-9 per request (1 through 65535).
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
        /// Wait for the full Stub activation marker before transferring.
        #[arg(long)]
        wait: bool,
    },
    /// Delete an OS-9 file on the CD-i player through a running full CD-i Stub.
    Delete {
        /// Absolute OS-9 path to the file to delete, for example
        /// /nvr/old-settings.bin.
        remote_path: String,
        /// Wait for the full Stub activation marker before deleting.
        #[arg(long)]
        wait: bool,
    },
    /// Mount /cd and /nvr using FUSE (Linux); only new files in /nvr can be written.
    Mount {
        /// Existing empty host directory used as mount point.
        mountpoint: String,
    },
    /// Upload a memory range from a running full CD-i Stub into a local file.
    Upload {
        /// Destination file. It is written only after a successful transfer.
        file: String,
        /// First CD-i memory address to upload (hexadecimal, as in CD-i Link).
        #[arg(long, value_parser = parse_address)]
        address: u32,
        /// Number of bytes to upload.
        #[arg(long)]
        size: usize,
        /// Maximum bytes per protocol READ request (1 through 65535).
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
        /// Ask the full Stub to switch to this baud rate before uploading. The
        /// Stub selects the highest supported rate not exceeding this value.
        #[arg(long)]
        upload_baud: Option<u32>,
        /// Wait for a full cdi_stub activation marker before uploading.
        #[arg(long)]
        wait: bool,
        /// Send END after the upload completes.
        #[arg(long)]
        end: bool,
    },
}

fn parse_address(text: &str) -> std::result::Result<u32, String> {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .or_else(|| text.strip_prefix('$'))
        .unwrap_or(text);
    u32::from_str_radix(hex, 16).map_err(|_| format!("{text:?} is not a valid 32-bit address"))
}

fn open(cli: &Cli) -> Result<Box<dyn serialport::SerialPort>> {
    let baud = if cli.mister { MISTER_BAUD } else { cli.baud };
    let mut port = serialport::new(&cli.port, baud)
        .data_bits(serialport::DataBits::Eight)
        .flow_control(serialport::FlowControl::None)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .dtr_on_open(true)
        .timeout(Duration::from_millis(cli.timeout_ms))
        .open()
        .with_context(|| format!("cannot open serial port {} at {baud} baud", cli.port))?;
    // The published CD-i null-modem cable feeds these host lines into the
    // player's CTS/RTS inputs. Windows' communications API enables them; do
    // the same explicitly instead of relying on a USB adapter's defaults.
    port.write_data_terminal_ready(true)
        .context("asserting DTR on serial port")?;
    port.write_request_to_send(true)
        .context("asserting RTS on serial port")?;
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::{cdfm_entries, parse_address};

    #[test]
    fn cdi_link_addresses_are_always_hexadecimal() {
        assert_eq!(parse_address("8000").unwrap(), 0x8000);
        assert_eq!(parse_address("0x8000").unwrap(), 0x8000);
        assert_eq!(parse_address("$8000").unwrap(), 0x8000);
    }

    #[test]
    fn cdfm_directory_records_expose_sector_size_and_name() {
        let mut record = Vec::new();
        record.extend_from_slice(&2275_u32.to_be_bytes());
        record.extend_from_slice(&[0; 4]);
        record.extend_from_slice(&370_u32.to_be_bytes());
        record.extend_from_slice(&[96, 5, 21, 20, 59, 37]);
        record.extend_from_slice(&[0; 8]);
        record.push(7);
        record.extend_from_slice(b"MODULES");
        assert_eq!(
            cdfm_entries(&record),
            vec![(2275, 370, "MODULES".to_owned())]
        );
    }
}

fn banner(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || matches!(b, b' ' | b'\r' | b'\n' | b'\t') {
                char::from(b).to_string()
            } else {
                format!("\\x{b:02X}")
            }
        })
        .collect()
}

fn progress_bar(label: &str, done: usize, total: usize) {
    const WIDTH: usize = 30;
    let done = done.min(total);
    let filled = if total == 0 {
        WIDTH
    } else {
        done * WIDTH / total
    };
    let percent = if total == 0 { 100 } else { done * 100 / total };
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));
    eprint!("\r{label} [{bar}] {percent:>3}% ({done}/{total} bytes)");
    let _ = std::io::stderr().flush();
}

/// A deliberately small, receive-only terminal mode. CD-i Link's `-terminal`
/// mode is useful for OS-9/application diagnostics that are written to the
/// serial port after `END` starts normal boot processing.
fn terminal<T: Read>(io: &mut T, mut log: Option<fs::File>) -> Result<()> {
    eprintln!("Serial terminal open; press Ctrl-C to exit.");
    let mut output = std::io::stdout();
    let mut buffer = [0_u8; 1024];
    loop {
        match io.read(&mut buffer) {
            Ok(0) => continue,
            Ok(count) => {
                output
                    .write_all(&buffer[..count])
                    .context("writing serial terminal output")?;
                output.flush().context("flushing serial terminal output")?;
                if let Some(log) = &mut log {
                    log.write_all(&buffer[..count])
                        .context("writing serial terminal log")?;
                    log.flush().context("flushing serial terminal log")?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error).context("reading serial terminal output"),
        }
    }
}

const OS9_I_OPEN: u16 = 0x84;
const OS9_I_CREATE: u16 = 0x83;
const OS9_I_DELETE: u16 = 0x87;
const OS9_I_READ: u16 = 0x89;
const OS9_I_WRITE: u16 = 0x8a;
const OS9_I_CLOSE: u16 = 0x8f;
const OS9_I_GETSTT: u16 = 0x8d;
const OS9_DIRECTORY_READ: u32 = 0x81;
const OS9_FILE_READ: u32 = 0x01;
const OS9_FILE_READ_WRITE: u32 = 0x03;
const OS9_OWNER_READ_WRITE: u32 = 0x03;
const OS9_DIRECTORY_ENTRY_SIZE: usize = 32;

fn cdfm_entries(data: &[u8]) -> Vec<(u32, u32, String)> {
    let mut entries = Vec::new();
    // CDFM presents compact-disc directory records. Their fixed prefix has a
    // big-endian sector at 0, byte size at 8, and name length at 26; unlike
    // RBF records they are variable-length and may cross I$Read boundaries.
    for start in 0..data.len().saturating_sub(27) {
        let name_len = usize::from(data[start + 26]);
        if name_len > 32 || start + 27 + name_len > data.len() {
            continue;
        }
        if data[start + 4..start + 8] != [0; 4] || data[start + 18..start + 26] != [0; 8] {
            continue;
        }
        let raw_name = &data[start + 27..start + 27 + name_len];
        if raw_name.is_empty() {
            continue;
        }
        if !raw_name
            .iter()
            .all(|byte| byte.is_ascii_graphic() && *byte != b'/')
            && !matches!(raw_name, [0] | [1])
        {
            continue;
        }
        let sector = u32::from_be_bytes(data[start..start + 4].try_into().unwrap());
        let size = u32::from_be_bytes(data[start + 8..start + 12].try_into().unwrap());
        if sector == 0 && size == 0 {
            continue;
        }
        let name = if matches!(raw_name, [0] | [1]) {
            ".".to_owned()
        } else {
            String::from_utf8_lossy(raw_name).into_owned()
        };
        entries.push((sector, size, name));
    }
    entries
}

fn print_directory_entries(data: &[u8]) -> usize {
    let cdfm = cdfm_entries(data);
    if !cdfm.is_empty() {
        println!("Sector      Size Name");
        println!("------ --------- -----");
        for (sector, size, name) in cdfm {
            println!("{sector:>6} {size:>9} {name}");
        }
        return 1;
    }

    let mut entries = 0;
    for entry in data.chunks_exact(OS9_DIRECTORY_ENTRY_SIZE) {
        let name_end = entry[..28].iter().position(|&byte| byte == 0).unwrap_or(28);
        if name_end == 0 {
            continue;
        }
        let name = String::from_utf8_lossy(&entry[..name_end]);
        let descriptor = u32::from_be_bytes([entry[28], entry[29], entry[30], entry[31]]);
        println!("{name}\t0x{descriptor:08X}");
        entries += 1;
    }
    entries
}

fn read_directory<T: Read + Write>(
    session: &mut Session<T>,
    path: &str,
    read_size: usize,
) -> Result<Vec<u8>> {
    if read_size == 0 || read_size > u16::MAX as usize || read_size % OS9_DIRECTORY_ENTRY_SIZE != 0
    {
        bail!("--read-size must be a non-zero multiple of {OS9_DIRECTORY_ENTRY_SIZE}, up to 65535");
    }
    if path.as_bytes().contains(&0) {
        bail!("directory path must not contain a NUL byte");
    }

    // CD-i Link explicitly selects address zero before its first full-Stub
    // BUFFER request. A freshly activated Stub does not otherwise guarantee a
    // useful current-address state.
    session
        .set_address(0)
        .context("initializing full Stub address")?;
    let mut path_bytes = path.as_bytes().to_vec();
    path_bytes.push(0);
    let (path_buffer_size, path_buffer) = session
        .allocate_buffer(path_bytes.len())
        .context("allocating OS-9 path buffer")?;
    if path_buffer_size < path_bytes.len() {
        bail!("full Stub allocated an unexpectedly small OS-9 path buffer");
    }
    session
        .write(&path_bytes)
        .context("writing OS-9 directory path")?;
    session
        .set_registers(REG_D0 | REG_A0, &[OS9_DIRECTORY_READ, path_buffer])
        .context("setting registers for OS-9 directory open")?;
    let open_result = session
        .os9_call(OS9_I_OPEN, 0)
        .context("opening OS-9 directory")?;
    if open_result & REG_CARRY != 0 {
        bail!("OS-9 could not open directory {path:?} (error reported by Stub)");
    }

    let (_, data_buffer) = session
        .allocate_buffer(read_size)
        .context("allocating OS-9 directory data buffer")?;
    // CD-i Link queries path status (SS.Opt, function value zero) before its
    // first directory read. Besides confirming this is a directory path, the
    // CD-i OS-9 file manager uses this query to initialise directory state.
    session
        .set_registers(REG_D1 | REG_A0, &[0, data_buffer])
        .context("setting registers for OS-9 directory status")?;
    let status_result = session
        .os9_call(OS9_I_GETSTT, 0)
        .context("querying OS-9 directory status")?;
    if status_result & REG_CARRY != 0 {
        bail!("OS-9 could not query directory status for {path:?}");
    }
    // The original client reads the first status byte before starting I$Read.
    // This also advances the Stub's selected register/status cursor.
    session
        .read(1)
        .context("reading OS-9 directory status byte")?;
    let result = (|| -> Result<Vec<u8>> {
        let mut directory_data = Vec::new();
        loop {
            // D0 remains the path number returned by I$Open and A0 remains
            // the buffer established by I$GetStt. CD-i Link updates D1 only
            // before each directory I$Read.
            session
                .set_registers(REG_D1, &[read_size as u32])
                .context("setting registers for OS-9 directory read")?;
            let read_result = session
                .os9_call(OS9_I_READ, 0)
                .context("reading OS-9 directory")?;
            if read_result & REG_CARRY != 0 {
                break;
            }
            // I$Read returns its valid-byte count through the low word of D1.
            // CD-i Link reads D0/D1 here before fetching the data buffer.
            session
                .select_registers(REG_D0 | REG_D1)
                .context("selecting OS-9 directory result registers")?;
            let registers = session
                .read(8)
                .context("reading OS-9 directory result registers")?;
            let d1 = u32::from_be_bytes(registers[4..8].try_into().unwrap());
            let valid_bytes = (d1 & 0xffff) as usize;
            if valid_bytes == 0 || valid_bytes > read_size {
                break;
            }
            session
                .set_address(data_buffer)
                .context("selecting OS-9 directory data buffer")?;
            let data = session
                .read(valid_bytes)
                .context("reading OS-9 directory data from Stub")?;
            directory_data.extend(data);
        }
        Ok(directory_data)
    })();
    let close_result = session.os9_call(OS9_I_CLOSE, 0);
    let directory_data = result?;
    close_result.context("closing OS-9 directory")?;
    Ok(directory_data)
}

fn print_directory<T: Read + Write>(
    session: &mut Session<T>,
    path: &str,
    read_size: usize,
) -> Result<()> {
    let directory_data = read_directory(session, path, read_size)?;
    if print_directory_entries(&directory_data) == 0 {
        eprintln!("Directory is empty.");
    }
    eprintln!("Done.");
    Ok(())
}

fn get_file<T: Read + Write>(
    session: &mut Session<T>,
    remote_path: &str,
    chunk_size: usize,
) -> Result<Vec<u8>> {
    if !(1..=u16::MAX as usize).contains(&chunk_size) {
        bail!("--chunk-size must be in 1..=65535");
    }
    if remote_path.as_bytes().contains(&0) {
        bail!("source path must not contain a NUL byte");
    }
    session
        .set_address(0)
        .context("initializing full Stub address")?;
    let mut path_bytes = remote_path.as_bytes().to_vec();
    path_bytes.push(0);
    let (path_buffer_size, path_buffer) = session
        .allocate_buffer(path_bytes.len())
        .context("allocating OS-9 source-path buffer")?;
    if path_buffer_size < path_bytes.len() {
        bail!("full Stub allocated an unexpectedly small OS-9 source-path buffer");
    }
    session
        .write(&path_bytes)
        .context("writing OS-9 source path")?;
    session
        .set_registers(REG_D0 | REG_A0, &[OS9_FILE_READ, path_buffer])
        .context("setting registers for OS-9 file open")?;
    let open_result = session
        .os9_call(OS9_I_OPEN, 0)
        .context("opening OS-9 source file")?;
    if open_result & REG_CARRY != 0 {
        bail!("OS-9 could not open source file {remote_path:?} (error reported by Stub)");
    }

    let (_, data_buffer) = session
        .allocate_buffer(chunk_size)
        .context("allocating OS-9 file data buffer")?;
    let result = (|| -> Result<Vec<u8>> {
        // A0 points to the file data buffer for the first I$Read. It remains
        // there for later reads, so only D1 needs updating in the loop.
        session
            .set_registers(REG_D1 | REG_A0, &[chunk_size as u32, data_buffer])
            .context("setting registers for OS-9 file read")?;
        let mut data = Vec::new();
        loop {
            let read_result = session
                .os9_call(OS9_I_READ, 0)
                .context("reading OS-9 source file")?;
            if read_result & REG_CARRY != 0 {
                break;
            }
            session
                .select_registers(REG_D0 | REG_D1)
                .context("selecting OS-9 file result registers")?;
            let registers = session
                .read(8)
                .context("reading OS-9 file result registers")?;
            let d1 = u32::from_be_bytes(registers[4..8].try_into().unwrap());
            let valid_bytes = (d1 & 0xffff) as usize;
            if valid_bytes == 0 || valid_bytes > chunk_size {
                break;
            }
            session
                .set_address(data_buffer)
                .context("selecting OS-9 file data buffer")?;
            data.extend(
                session
                    .read(valid_bytes)
                    .context("reading OS-9 file data from Stub")?,
            );
            session
                .set_registers(REG_D1, &[chunk_size as u32])
                .context("setting registers for next OS-9 file read")?;
        }
        Ok(data)
    })();
    let close_result = session.os9_call(OS9_I_CLOSE, 0);
    let data = result?;
    close_result.context("closing OS-9 source file")?;
    Ok(data)
}

fn put_file<T: Read + Write>(
    session: &mut Session<T>,
    local_data: &[u8],
    remote_path: &str,
    chunk_size: usize,
) -> Result<()> {
    if !(1..=u16::MAX as usize).contains(&chunk_size) {
        bail!("--chunk-size must be in 1..=65535");
    }
    if remote_path.as_bytes().contains(&0) {
        bail!("destination path must not contain a NUL byte");
    }
    session
        .set_address(0)
        .context("initializing full Stub address")?;
    let mut path_bytes = remote_path.as_bytes().to_vec();
    path_bytes.push(0);
    let (path_buffer_size, path_buffer) = session
        .allocate_buffer(path_bytes.len())
        .context("allocating OS-9 destination-path buffer")?;
    if path_buffer_size < path_bytes.len() {
        bail!("full Stub allocated an unexpectedly small OS-9 destination-path buffer");
    }
    session
        .write(&path_bytes)
        .context("writing OS-9 destination path")?;
    // I$Create takes D0 access mode, D1 attributes, and A0 path. Give the
    // Stub's OS-9 process owner read/write permission on the new regular
    // file; otherwise I$Write returns E$FNA (file not accessible).
    session
        .set_registers(
            REG_D0 | REG_D1 | REG_A0,
            &[OS9_FILE_READ_WRITE, OS9_OWNER_READ_WRITE, path_buffer],
        )
        .context("setting registers for OS-9 file create")?;
    let create_result = session
        .os9_call(OS9_I_CREATE, 0)
        .context("creating OS-9 destination file")?;
    if create_result & REG_CARRY != 0 {
        bail!("OS-9 could not create destination file {remote_path:?}; it may already exist");
    }

    let (_, data_buffer) = session
        .allocate_buffer(chunk_size)
        .context("allocating OS-9 file write buffer")?;
    let result = (|| -> Result<()> {
        for chunk in local_data.chunks(chunk_size) {
            session
                .set_registers(REG_D1 | REG_A0, &[chunk.len() as u32, data_buffer])
                .context("setting registers for OS-9 file write")?;
            session
                .set_address(data_buffer)
                .context("selecting OS-9 file write buffer")?;
            session
                .write(chunk)
                .context("copying host file data to Stub buffer")?;
            let write_result = session
                .os9_call(OS9_I_WRITE, 0)
                .context("writing OS-9 destination file")?;
            if write_result & REG_CARRY != 0 {
                session
                    .select_registers(REG_D1)
                    .context("selecting OS-9 write error register")?;
                let error = session
                    .read(4)
                    .context("reading OS-9 write error register")?;
                let error = u32::from_be_bytes(error.try_into().unwrap());
                bail!("OS-9 reported error 0x{error:08X} while writing {remote_path:?}");
            }
        }
        Ok(())
    })();
    let close_result = session.os9_call(OS9_I_CLOSE, 0);
    result?;
    close_result.context("closing OS-9 destination file")?;
    Ok(())
}

fn delete_file<T: Read + Write>(session: &mut Session<T>, remote_path: &str) -> Result<()> {
    if !remote_path.starts_with('/') {
        bail!("delete requires an absolute OS-9 path, such as /nvr/old-file");
    }
    if remote_path.as_bytes().contains(&0) {
        bail!("path to delete must not contain a NUL byte");
    }
    session
        .set_address(0)
        .context("initializing full Stub address")?;
    let mut path_bytes = remote_path.as_bytes().to_vec();
    path_bytes.push(0);
    let (path_buffer_size, path_buffer) = session
        .allocate_buffer(path_bytes.len())
        .context("allocating OS-9 delete-path buffer")?;
    if path_buffer_size < path_bytes.len() {
        bail!("full Stub allocated an unexpectedly small OS-9 delete-path buffer");
    }
    session
        .write(&path_bytes)
        .context("writing OS-9 delete path")?;
    // OS-9 ignores D0's access mode when A0 contains an absolute pathlist,
    // but supplying update access also matches the documented I$Delete call.
    session
        .set_registers(REG_D0 | REG_A0, &[OS9_FILE_READ_WRITE, path_buffer])
        .context("setting registers for OS-9 file delete")?;
    let delete_result = session
        .os9_call(OS9_I_DELETE, 0)
        .context("deleting OS-9 file")?;
    if delete_result & REG_CARRY != 0 {
        session
            .select_registers(REG_D1)
            .context("selecting OS-9 delete error register")?;
        let error = session
            .read(4)
            .context("reading OS-9 delete error register")?;
        let error = u32::from_be_bytes(error.try_into().unwrap());
        bail!("OS-9 could not delete {remote_path:?} (error 0x{error:08X})");
    }
    Ok(())
}

const FUSE_TTL: Duration = Duration::from_secs(1);
fn mount_owner_uid() -> u32 {
    // FUSE reports these attributes to the host kernel. Make the mounted view
    // owned by the user who started cdi-serial so its writable /nvr directory
    // is usable without elevated privileges.
    unsafe { libc::geteuid() }
}

fn mount_owner_gid() -> u32 {
    unsafe { libc::getegid() }
}

struct PendingFile {
    path: String,
    data: Vec<u8>,
    committed: bool,
}

struct CdiFuse {
    session: Mutex<Session<Box<dyn serialport::SerialPort>>>,
    paths: Mutex<HashMap<u64, String>>,
    directories: Mutex<HashMap<String, Vec<String>>>,
    sizes: Mutex<HashMap<String, u64>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    pending: Mutex<HashMap<u64, PendingFile>>,
}

impl CdiFuse {
    fn attr(ino: u64, directory: bool, size: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory { 0o755 } else { 0o644 },
            nlink: 1,
            uid: mount_owner_uid(),
            gid: mount_owner_gid(),
            rdev: 0,
            blksize: 256,
            flags: 0,
        }
    }
    fn path(&self, ino: u64) -> Option<String> {
        self.paths.lock().ok()?.get(&ino).cloned()
    }
    fn inode(&self, path: String) -> u64 {
        let mut paths = self.paths.lock().unwrap();
        if let Some((&ino, _)) = paths.iter().find(|(_, value)| **value == path) {
            return ino;
        }
        let ino = paths.len() as u64 + 2;
        paths.insert(ino, path);
        ino
    }
    fn names(&self, path: &str) -> std::result::Result<Vec<String>, ()> {
        if let Some(names) = self.directories.lock().unwrap().get(path).cloned() {
            return Ok(names);
        }
        let mut session = self.session.lock().map_err(|_| ())?;
        let data = read_directory(&mut *session, path, 256).map_err(|_| ())?;
        let cdfm = cdfm_entries(&data);
        let names: Vec<String> = if !cdfm.is_empty() {
            let mut sizes = self.sizes.lock().unwrap();
            cdfm.into_iter()
                .filter_map(|(_, size, name)| {
                    (name != ".").then(|| {
                        sizes.insert(format!("{path}/{name}"), size as u64);
                        name
                    })
                })
                .collect()
        } else {
            data.chunks_exact(32)
                .filter_map(|entry| {
                    let end = entry[..28].iter().position(|&b| b == 0)?;
                    (end > 0).then(|| String::from_utf8_lossy(&entry[..end]).into_owned())
                })
                .collect()
        };
        self.directories
            .lock()
            .unwrap()
            .insert(path.to_owned(), names.clone());
        Ok(names)
    }
    fn size(&self, path: &str) -> Option<u64> {
        self.sizes.lock().ok()?.get(path).copied()
    }
    fn file(&self, path: &str) -> std::result::Result<Vec<u8>, ()> {
        if let Some(data) = self.files.lock().unwrap().get(path).cloned() {
            return Ok(data);
        }
        let mut session = self.session.lock().map_err(|_| ())?;
        let data = get_file(&mut *session, path, 256).map_err(|_| ())?;
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), data.clone());
        Ok(data)
    }
    fn pending_data(&self, fh: u64) -> Option<Vec<u8>> {
        self.pending
            .lock()
            .ok()?
            .get(&fh)
            .map(|file| file.data.clone())
    }
    fn pending_handle(&self, fh: u64, ino: u64) -> Option<u64> {
        let pending = self.pending.lock().ok()?;
        if pending.contains_key(&fh) {
            Some(fh)
        } else {
            pending.contains_key(&ino).then_some(ino)
        }
    }
    fn commit_pending(&self, fh: u64) -> std::result::Result<(), ()> {
        let (path, data) = {
            let pending = self.pending.lock().map_err(|_| ())?;
            let file = pending.get(&fh).ok_or(())?;
            if file.committed {
                return Ok(());
            }
            (file.path.clone(), file.data.clone())
        };
        let mut session = self.session.lock().map_err(|_| ())?;
        put_file(&mut *session, &data, &path, 256).map_err(|_| ())?;
        self.pending
            .lock()
            .map_err(|_| ())?
            .get_mut(&fh)
            .ok_or(())?
            .committed = true;
        self.files
            .lock()
            .map_err(|_| ())?
            .insert(path.clone(), data.clone());
        self.sizes
            .lock()
            .map_err(|_| ())?
            .insert(path.clone(), data.len() as u64);
        if let Some(name) = path.rsplit('/').next() {
            let mut directories = self.directories.lock().map_err(|_| ())?;
            let names = directories.entry("/nvr".to_owned()).or_default();
            if !names.iter().any(|entry| entry == name) {
                names.push(name.to_owned());
            }
        }
        Ok(())
    }
}
impl Filesystem for CdiFuse {
    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_string_lossy();
        let parent_path = self.path(parent.0).unwrap_or_else(|| "/".into());
        let path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };
        if parent_path == "/" && (name == "cd" || name == "nvr") {
            let ino = self.inode(path);
            reply.entry(&FUSE_TTL, &Self::attr(ino, true, 0), Generation(0));
            return;
        }
        match self.names(&parent_path) {
            Ok(names) if names.iter().any(|item| item == name.as_ref()) => {
                let ino = self.inode(path.clone());
                if let Some(data) = self.pending_data(ino) {
                    reply.entry(
                        &FUSE_TTL,
                        &Self::attr(ino, false, data.len() as u64),
                        Generation(0),
                    );
                } else if let Some(size) = self.size(&path) {
                    reply.entry(&FUSE_TTL, &Self::attr(ino, false, size), Generation(0));
                } else {
                    match self.file(&path) {
                        Ok(data) => reply.entry(
                            &FUSE_TTL,
                            &Self::attr(ino, false, data.len() as u64),
                            Generation(0),
                        ),
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
            }
            _ => reply.error(Errno::ENOENT),
        }
    }
    fn getattr(&self, _: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        let path = self.path(ino.0).unwrap_or_else(|| "/".into());
        if path == "/" || path == "/cd" || path == "/nvr" {
            reply.attr(&FUSE_TTL, &Self::attr(ino.0, true, 0));
        } else if let Some(data) = self.pending_data(ino.0) {
            reply.attr(&FUSE_TTL, &Self::attr(ino.0, false, data.len() as u64));
        } else if let Some(size) = self.size(&path) {
            reply.attr(&FUSE_TTL, &Self::attr(ino.0, false, size));
        } else {
            match self.file(&path) {
                Ok(data) => reply.attr(&FUSE_TTL, &Self::attr(ino.0, false, data.len() as u64)),
                Err(_) => reply.error(Errno::ENOENT),
            }
        }
    }
    fn open(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if let Some(handle) = self.pending_handle(ino.0, ino.0) {
            reply.opened(FileHandle(handle), FopenFlags::empty());
            return;
        }
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }
    fn read(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .pending_data(ino.0)
            .or_else(|| self.path(ino.0).and_then(|p| self.file(&p).ok()))
        {
            Some(data) => {
                let start = (offset as usize).min(data.len());
                let end = (start + size as usize).min(data.len());
                reply.data(&data[start..end]);
            }
            None => reply.error(Errno::ENOENT),
        }
    }
    fn create(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        _: u32,
        _: u32,
        _: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = self.path(parent.0).unwrap_or_else(|| "/".into());
        let name = name.to_string_lossy();
        if parent_path == "/cd" {
            reply.error(Errno::EROFS);
            return;
        }
        if parent_path != "/nvr" {
            reply.error(Errno::EACCES);
            return;
        }
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            reply.error(Errno::EINVAL);
            return;
        }
        if name.len() > 28 {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }
        let path = format!("/nvr/{name}");
        match self.names("/nvr") {
            Ok(names) if names.iter().any(|entry| entry == name.as_ref()) => {
                reply.error(Errno::EEXIST);
            }
            Err(_) => reply.error(Errno::EIO),
            Ok(_) => {
                let ino = self.inode(path.clone());
                self.pending.lock().unwrap().insert(
                    ino,
                    PendingFile {
                        path,
                        data: Vec::new(),
                        committed: false,
                    },
                );
                reply.created(
                    &FUSE_TTL,
                    &Self::attr(ino, false, 0),
                    Generation(0),
                    FileHandle(ino),
                    FopenFlags::empty(),
                );
            }
        }
    }
    fn write(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Ok(offset) = usize::try_from(offset) else {
            reply.error(Errno::EFBIG);
            return;
        };
        let Some(end) = offset.checked_add(data.len()) else {
            reply.error(Errno::EFBIG);
            return;
        };
        let Some(handle) = self.pending_handle(fh.0, ino.0) else {
            reply.error(Errno::EROFS);
            return;
        };
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let Some(file) = pending.get_mut(&handle) else {
            reply.error(Errno::EROFS);
            return;
        };
        if file.committed {
            reply.error(Errno::EROFS);
            return;
        }
        if file.data.len() < end {
            file.data.resize(end, 0);
        }
        file.data[offset..end].copy_from_slice(data);
        reply.written(data.len() as u32);
    }
    fn flush(&self, _: &Request, _: INodeNo, _: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        // FUSE may issue flush before its final buffered WRITE request. The
        // CD-i file service can create a file only once, so defer its one-shot
        // I$Create/I$Write transaction until release (the final close).
        reply.ok();
    }
    fn release(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.pending_handle(fh.0, ino.0);
        let result = handle
            .map(|handle| self.commit_pending(handle))
            .unwrap_or(Ok(()));
        if let Some(handle) = handle {
            self.pending
                .lock()
                .ok()
                .and_then(|mut files| files.remove(&handle));
        }
        match result {
            Ok(()) => reply.ok(),
            Err(()) => reply.error(Errno::EIO),
        }
    }
    fn unlink(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = self.path(parent.0).unwrap_or_else(|| "/".into());
        let name = name.to_string_lossy();
        if parent_path == "/cd" {
            reply.error(Errno::EROFS);
            return;
        }
        if parent_path != "/nvr" {
            reply.error(Errno::EACCES);
            return;
        }
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            reply.error(Errno::EINVAL);
            return;
        }
        let path = format!("/nvr/{name}");
        if self
            .pending
            .lock()
            .ok()
            .is_some_and(|pending| pending.values().any(|file| file.path == path))
        {
            reply.error(Errno::EBUSY);
            return;
        }
        let result = self
            .session
            .lock()
            .map_err(|_| ())
            .and_then(|mut session| delete_file(&mut *session, &path).map_err(|_| ()));
        match result {
            Ok(()) => {
                self.files
                    .lock()
                    .ok()
                    .and_then(|mut files| files.remove(&path));
                self.sizes
                    .lock()
                    .ok()
                    .and_then(|mut sizes| sizes.remove(&path));
                if let Ok(mut directories) = self.directories.lock() {
                    if let Some(names) = directories.get_mut("/nvr") {
                        names.retain(|entry| entry != name.as_ref());
                    }
                }
                reply.ok();
            }
            Err(()) => reply.error(Errno::EIO),
        }
    }
    fn readdir(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let path = self.path(ino.0).unwrap_or_else(|| "/".into());
        let names = if path == "/" {
            Ok(vec!["cd".into(), "nvr".into()])
        } else {
            self.names(&path)
        };
        match names {
            Ok(names) => {
                let mut entries = vec![
                    (ino.0, FileType::Directory, ".".to_owned()),
                    (1, FileType::Directory, "..".to_owned()),
                ];
                entries.extend(names.into_iter().map(|name| {
                    let child = if path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{path}/{name}")
                    };
                    let kind = if path == "/" {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    (self.inode(child), kind, name)
                }));
                for (index, (child, kind, name)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(INodeNo(child), (index + 1) as u64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.mister {
        match &cli.command {
            Command::Download {
                terminal_baud: Some(_),
                ..
            } => {
                bail!(
                    "--terminal-baud cannot be used with --mister; MiSTer mode is fixed at {MISTER_BAUD} baud"
                );
            }
            Command::Upload {
                upload_baud: Some(_),
                ..
            } => {
                bail!(
                    "--upload-baud cannot be used with --mister; MiSTer mode is fixed at {MISTER_BAUD} baud"
                );
            }
            _ => {}
        }
        eprintln!("MiSTer mode: holding serial port at {MISTER_BAUD} baud.");
    }
    let reset_requested = matches!(
        &cli.command,
        Command::Download { reset: true, .. } | Command::Stub { .. }
    );
    if reset_requested {
        // Keep this as a separate open/write/close sequence. It intentionally
        // mirrors the established `stty; echo '\x03' > /dev/ttyUSB0; cdilink`
        // workflow, including its modem-control line transition on close.
        let mut reset_port = open(&cli)?;
        reset_port
            .write_all(&[0x03])
            .context("sending Ctrl-C reset byte")?;
        reset_port.flush().context("flushing Ctrl-C reset byte")?;
    }
    let port = open(&cli)?;
    let mut session = Session::new(port);
    match &cli.command {
        Command::Wait { max_banner_bytes } => {
            let greeting = session
                .wait_for_stub(*max_banner_bytes)
                .context("waiting for CD-i Stub")?;
            print!("{}", banner(&greeting));
        }
        Command::Download {
            file,
            address,
            chunk_size,
            wait,
            reset,
            execute,
            wait_for_return,
            end,
            terminal: terminal_requested,
            terminal_baud,
            terminal_log,
        } => {
            if !(1..=u16::MAX as usize).contains(chunk_size) {
                bail!("--chunk-size must be in 1..=65535");
            }
            let image = fs::read(file).with_context(|| format!("cannot read image {file}"))?;
            if *wait || *reset {
                eprintln!("Waiting for CD-i download subset...");
                let greeting = session
                    .wait_for_stub(4096)
                    .context("waiting for CD-i Stub/download subset")?;
                let greeting = banner(&greeting);
                if greeting.trim().is_empty() {
                    eprintln!("Download subset active.");
                    if !cli.mister {
                        session
                            .transport_mut()
                            .set_baud_rate(19_200)
                            .context("switching to 19200 baud after download-subset handshake")?;
                    }
                } else {
                    eprintln!("Stub active: {}", greeting.trim());
                }
                // The original CD-i Link waits 500 ms after its ready-marker
                // receive path before issuing the first request.
                std::thread::sleep(Duration::from_millis(500));
            }
            progress_bar("Downloading", 0, image.len());
            session
                .download_with_progress(*address, &image, *chunk_size, |done| {
                    progress_bar("Downloading", done, image.len())
                })
                .context("download failed")?;
            eprintln!();
            if *execute {
                session.execute(*address).context("execute failed")?;
                if *wait_for_return {
                    session
                        .wait_for_execution_end()
                        .context("waiting for application return")?;
                }
            }
            if *end {
                session.end().context("ending stub")?;
            }
            eprintln!("Done.");
            if *terminal_requested {
                if let Some(baud) = terminal_baud {
                    session
                        .transport_mut()
                        .set_baud_rate(*baud)
                        .with_context(|| format!("switching serial terminal to {baud} baud"))?;
                }
                let log = terminal_log
                    .as_deref()
                    .map(|path| {
                        fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .with_context(|| format!("opening serial terminal log {path}"))
                    })
                    .transpose()?;
                terminal(session.transport_mut(), log)?;
            }
        }
        Command::Stub {
            file,
            address,
            chunk_size,
        } => {
            if !(1..=u16::MAX as usize).contains(chunk_size) {
                bail!("--chunk-size must be in 1..=65535");
            }
            let image =
                fs::read(file).with_context(|| format!("cannot read Stub module {file}"))?;
            eprintln!("Waiting for CD-i download subset...");
            let greeting = session
                .wait_for_stub(4096)
                .context("waiting for CD-i Stub/download subset")?;
            let greeting = banner(&greeting);
            if !greeting.trim().is_empty() {
                bail!("a full Stub is already active; do not bootstrap another one");
            }
            eprintln!("Download subset active.");
            if !cli.mister {
                session
                    .transport_mut()
                    .set_baud_rate(19_200)
                    .context("switching to 19200 baud after download-subset handshake")?;
            }
            std::thread::sleep(Duration::from_millis(500));
            progress_bar("Downloading Stub", 0, image.len());
            session
                .download_with_progress(*address, &image, *chunk_size, |done| {
                    progress_bar("Downloading Stub", done, image.len())
                })
                .context("Stub download failed")?;
            eprintln!();
            session.end().context("starting full Stub")?;
            eprintln!("Full Stub started. Use `upload` to read CD-i memory.");
        }
        Command::Dir {
            path,
            wait,
            read_size,
        } => {
            if *wait {
                eprintln!("Waiting for full CD-i Stub...");
                let greeting = session
                    .wait_for_stub(4096)
                    .context("waiting for full CD-i Stub")?;
                let greeting = banner(&greeting);
                if greeting.trim().is_empty() {
                    bail!(
                        "ROM download subset is active; directory operations require a full cdi_stub"
                    );
                }
                eprintln!("Stub active: {}", greeting.trim());
            }
            print_directory(&mut session, path, *read_size)?;
        }
        Command::Get {
            remote_path,
            local_file,
            chunk_size,
            wait,
        } => {
            if *wait {
                eprintln!("Waiting for full CD-i Stub...");
                let greeting = session
                    .wait_for_stub(4096)
                    .context("waiting for full CD-i Stub")?;
                let greeting = banner(&greeting);
                if greeting.trim().is_empty() {
                    bail!("ROM download subset is active; file transfers require a full cdi_stub");
                }
                eprintln!("Stub active: {}", greeting.trim());
            }
            let data = get_file(&mut session, remote_path, *chunk_size)?;
            fs::write(local_file, &data)
                .with_context(|| format!("writing downloaded file to {local_file}"))?;
            eprintln!("Copied {} bytes to {local_file}.", data.len());
        }
        Command::Put {
            local_file,
            remote_path,
            chunk_size,
            wait,
        } => {
            if *wait {
                eprintln!("Waiting for full CD-i Stub...");
                let greeting = session
                    .wait_for_stub(4096)
                    .context("waiting for full CD-i Stub")?;
                let greeting = banner(&greeting);
                if greeting.trim().is_empty() {
                    bail!("ROM download subset is active; file transfers require a full cdi_stub");
                }
                eprintln!("Stub active: {}", greeting.trim());
            }
            let data = fs::read(local_file)
                .with_context(|| format!("reading host source file {local_file}"))?;
            put_file(&mut session, &data, remote_path, *chunk_size)?;
            eprintln!("Copied {} bytes to {remote_path}.", data.len());
        }
        Command::Delete { remote_path, wait } => {
            if *wait {
                eprintln!("Waiting for full CD-i Stub...");
                let greeting = session
                    .wait_for_stub(4096)
                    .context("waiting for full CD-i Stub")?;
                let greeting = banner(&greeting);
                if greeting.trim().is_empty() {
                    bail!("ROM download subset is active; file deletion requires a full cdi_stub");
                }
                eprintln!("Stub active: {}", greeting.trim());
            }
            delete_file(&mut session, remote_path)?;
            eprintln!("Deleted {remote_path}.");
        }
        Command::Mount { mountpoint } => {
            let mut paths = HashMap::new();
            paths.insert(1, "/".to_owned());
            let fs = CdiFuse {
                session: Mutex::new(session),
                paths: Mutex::new(paths),
                directories: Mutex::new(HashMap::new()),
                sizes: Mutex::new(HashMap::new()),
                files: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
            };
            eprintln!(
                "Mounting CD-i filesystem at {mountpoint}; only new /nvr files are writable. Unmount with fusermount3 -u {mountpoint}."
            );
            let mut config = Config::default();
            config.mount_options = vec![
                MountOption::RW,
                MountOption::DefaultPermissions,
                MountOption::FSName("cdi-serial".into()),
            ];
            fuser::mount(fs, mountpoint, &config).context("mounting FUSE filesystem")?;
        }
        Command::Upload {
            file,
            address,
            size,
            chunk_size,
            upload_baud,
            wait,
            end,
        } => {
            if *size == 0 {
                bail!("--size must be greater than zero");
            }
            if !(1..=u16::MAX as usize).contains(chunk_size) {
                bail!("--chunk-size must be in 1..=65535");
            }
            if *wait {
                eprintln!("Waiting for full CD-i Stub...");
                let greeting = session
                    .wait_for_stub(4096)
                    .context("waiting for full CD-i Stub")?;
                let greeting = banner(&greeting);
                if greeting.trim().is_empty() {
                    bail!(
                        "ROM download subset is active; it cannot read memory. Start a full cdi_stub first"
                    );
                }
                eprintln!("Stub active: {}", greeting.trim());
            }
            if let Some(baud) = upload_baud {
                let selected = session
                    .negotiate_baud_rate(*baud)
                    .context("negotiating upload baud rate")?;
                if selected == 0 {
                    bail!("the running Stub does not support baud-rate switching");
                }
                session
                    .transport_mut()
                    .set_baud_rate(selected)
                    .with_context(|| format!("switching local serial port to {selected} baud"))?;
                eprintln!("Upload baud rate: {selected}");
            }
            progress_bar("Uploading", 0, *size);
            let data = session
                .upload_with_progress(*address, *size, *chunk_size, |done| {
                    progress_bar("Uploading", done, *size)
                })
                .context("upload failed")?;
            eprintln!();
            fs::write(file, data)
                .with_context(|| format!("writing downloaded memory to {file}"))?;
            if *end {
                session.end().context("ending stub")?;
            }
            eprintln!("Done.");
        }
    }
    Ok(())
}
