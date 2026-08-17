use std::{fs, io::Write, time::Duration};

use anyhow::{Context, Result, bail};
use cdilink::Session;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "cdilink",
    version,
    about = "Upload applications over the CD-i Stub serial protocol"
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
    /// Write an application image to memory, optionally start it, then end the stub.
    Upload {
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
        /// Send END after upload. On the built-in download subset this resumes boot.
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
    let mut port = serialport::new(&cli.port, cli.baud)
        .data_bits(serialport::DataBits::Eight)
        .flow_control(serialport::FlowControl::None)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .dtr_on_open(true)
        .timeout(Duration::from_millis(cli.timeout_ms))
        .open()
        .with_context(|| format!("cannot open serial port {} at {} baud", cli.port, cli.baud))?;
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

fn progress_bar(done: usize, total: usize) {
    const WIDTH: usize = 30;
    let done = done.min(total);
    let filled = if total == 0 {
        WIDTH
    } else {
        done * WIDTH / total
    };
    let percent = if total == 0 { 100 } else { done * 100 / total };
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));
    eprint!("\rUploading [{bar}] {percent:>3}% ({done}/{total} bytes)");
    let _ = std::io::stderr().flush();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let reset_requested = matches!(&cli.command, Command::Upload { reset: true, .. });
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
        Command::Upload {
            file,
            address,
            chunk_size,
            wait,
            reset,
            execute,
            wait_for_return,
            end,
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
                    session
                        .transport_mut()
                        .set_baud_rate(19_200)
                        .context("switching to 19200 baud after download-subset handshake")?;
                } else {
                    eprintln!("Stub active: {}", greeting.trim());
                }
                // The original CD-i Link waits 500 ms after its ready-marker
                // receive path before issuing the first request.
                std::thread::sleep(Duration::from_millis(500));
            }
            progress_bar(0, image.len());
            session
                .upload_with_progress(*address, &image, *chunk_size, |done| {
                    progress_bar(done, image.len())
                })
                .context("upload failed")?;
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
        }
    }
    Ok(())
}
