use std::{
    fs,
    io::{Read, Write},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use cdi_serial::Session;
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
    use super::parse_address;

    #[test]
    fn cdi_link_addresses_are_always_hexadecimal() {
        assert_eq!(parse_address("8000").unwrap(), 0x8000);
        assert_eq!(parse_address("0x8000").unwrap(), 0x8000);
        assert_eq!(parse_address("$8000").unwrap(), 0x8000);
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
    let reset_requested = matches!(&cli.command, Command::Download { reset: true, .. });
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
