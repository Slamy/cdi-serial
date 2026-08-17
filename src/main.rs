use std::{
    fs,
    io::{Read, Write},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use cdi_serial::{REG_A0, REG_CARRY, REG_D0, REG_D1, Session};
use clap::{Parser, Subcommand};

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
const OS9_I_READ: u16 = 0x89;
const OS9_I_CLOSE: u16 = 0x8f;
const OS9_I_GETSTT: u16 = 0x8d;
const OS9_DIRECTORY_READ: u32 = 0x81;
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

fn print_directory<T: Read + Write>(
    session: &mut Session<T>,
    path: &str,
    read_size: usize,
) -> Result<()> {
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
    if print_directory_entries(&directory_data) == 0 {
        eprintln!("Directory is empty.");
    }
    eprintln!("Done.");
    Ok(())
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
