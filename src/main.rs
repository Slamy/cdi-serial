use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use cdi_serial::{REG_A0, REG_CARRY, REG_D0, REG_D1, Session};
use clap::{Parser, Subcommand};
#[cfg(all(unix, feature = "fuse"))]
mod fuse;
mod os9;

use os9::{delete_file, get_file, print_directory, put_file};

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
    /// Print protocol and OS-9 operation diagnostics to stderr.
    #[arg(short, long)]
    verbose: bool,
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
    /// Dump or verify the CD-i system ROM through a running full CD-i Stub.
    Rom {
        #[command(subcommand)]
        command: RomCommand,
    },
    /// List OS-9-visible system and VMPEG ROM groups through a running full
    /// CD-i Stub.
    #[command(name = "romlist")]
    RomList {
        /// Wait for a full cdi_stub activation marker before reading.
        #[arg(long)]
        wait: bool,
        /// Send END after listing the ROM.
        #[arg(long)]
        end: bool,
    },
    /// List OS-9 modules and their live header attributes through a running
    /// full CD-i Stub.
    #[command(name = "mod")]
    Mod {
        /// Wait for a full cdi_stub activation marker before reading.
        #[arg(long)]
        wait: bool,
        /// Send END after listing modules.
        #[arg(long)]
        end: bool,
    },
    /// Mount /cd and /nvr using FUSE (Unix); only new files in /nvr can be written.
    #[cfg(all(unix, feature = "fuse"))]
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

#[derive(Debug, Subcommand)]
enum RomCommand {
    /// Dump a ROM range. Defaults match the standard 512 KiB CD-i system ROM.
    Dump {
        /// Destination ROM image on the host.
        file: String,
        /// First ROM address (hexadecimal).
        #[arg(long, default_value = "400000", value_parser = parse_address)]
        address: u32,
        /// Bytes to dump.
        #[arg(long, default_value_t = 524_288)]
        size: usize,
        /// Maximum bytes per protocol READ request.
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
        /// Ask the Stub to switch to this baud rate before dumping.
        #[arg(long)]
        upload_baud: Option<u32>,
        /// Wait for a full cdi_stub activation marker before dumping.
        #[arg(long)]
        wait: bool,
        /// Send END after dumping.
        #[arg(long)]
        end: bool,
    },
    /// Compare the player ROM against a local ROM dump byte-for-byte.
    Verify {
        /// Local ROM image used as the expected contents.
        file: String,
        /// First ROM address (hexadecimal).
        #[arg(long, default_value = "400000", value_parser = parse_address)]
        address: u32,
        /// Maximum bytes per protocol READ request.
        #[arg(long, default_value_t = 256)]
        chunk_size: usize,
        /// Ask the Stub to switch to this baud rate before verifying.
        #[arg(long)]
        upload_baud: Option<u32>,
        /// Wait for a full cdi_stub activation marker before verifying.
        #[arg(long)]
        wait: bool,
        /// Send END after verifying.
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
    use super::{ModuleDirectoryEntry, crc32_ieee, parse_address, system_rom_group};
    use crate::os9::cdfm_entries;

    #[test]
    fn cdi_link_addresses_are_always_hexadecimal() {
        assert_eq!(parse_address("8000").unwrap(), 0x8000);
        assert_eq!(parse_address("0x8000").unwrap(), 0x8000);
        assert_eq!(parse_address("$8000").unwrap(), 0x8000);
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32_ieee(b"123456789"), 0xcbf4_3926);
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

    #[test]
    fn system_rom_group_is_selected_from_live_module_mappings() {
        let system_modules = (0..4).map(|offset| ModuleDirectoryEntry {
            module: 0x0040_1000 + offset * 0x100,
            group: 0x0040_0000,
            group_size: 0x0008_0000,
            links: 1,
            checksum: 0,
        });
        let transient = ModuleDirectoryEntry {
            module: 0x0000_8000,
            group: 0x0000_8000,
            group_size: 0x0000_07cc,
            links: 1,
            checksum: 0,
        };
        let entries = system_modules
            .chain(std::iter::once(transient))
            .collect::<Vec<_>>();

        assert_eq!(system_rom_group(&entries), Some((0x0040_0000, 0x0008_0000)));
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

fn progress_bar_with_unit(label: &str, done: usize, total: usize, unit: &str) {
    const WIDTH: usize = 30;
    let done = done.min(total);
    let filled = if total == 0 {
        WIDTH
    } else {
        done * WIDTH / total
    };
    let percent = if total == 0 { 100 } else { done * 100 / total };
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));
    eprint!("\r{label} [{bar}] {percent:>3}% ({done}/{total} {unit})");
    let _ = std::io::stderr().flush();
}

fn progress_bar(label: &str, done: usize, total: usize) {
    progress_bar_with_unit(label, done, total, "bytes");
}

fn progress_items(label: &str, done: usize, total: usize) {
    progress_bar_with_unit(label, done, total, "modules");
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

// CD-i OS-9's F$GModDr service fills an A0/D1 supplied buffer with module
// directory records. This is the service used by CD-i Link before romlist.
const OS9_F_GMODDR: u16 = 0x1a;

fn os9_trace(enabled: bool, message: impl std::fmt::Display) {
    if enabled {
        eprintln!("[os9] {message}");
    }
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleDirectoryEntry {
    module: u32,
    group: u32,
    group_size: u32,
    links: u16,
    checksum: u16,
}

fn module_directory<T: Read + Write>(
    session: &mut Session<T>,
) -> Result<Vec<ModuleDirectoryEntry>> {
    session
        .set_address(0)
        .context("initializing full Stub address for module directory")?;
    let (capacity, buffer) = session
        .allocate_buffer(4096)
        .context("allocating OS-9 module-directory buffer")?;
    session
        .set_registers(REG_D1 | REG_A0, &[capacity as u32, buffer])
        .context("setting registers for F$GModDr")?;
    let result = session
        .os9_call(OS9_F_GMODDR, 0)
        .context("calling OS-9 F$GModDr")?;
    if result & REG_CARRY != 0 {
        bail!("OS-9 F$GModDr failed (error reported by Stub)");
    }
    session
        .select_registers(REG_D1)
        .context("selecting F$GModDr result size")?;
    let length = u32::from_be_bytes(
        session
            .read(4)
            .context("reading F$GModDr result size")?
            .try_into()
            .unwrap(),
    );
    let length = usize::try_from(length).context("converting F$GModDr result size")?;
    if length == 0 || length > capacity as usize || length % 16 != 0 {
        bail!("invalid F$GModDr result size {length}");
    }
    session
        .set_address(buffer)
        .context("selecting OS-9 module-directory buffer")?;
    let data = session
        .read(length)
        .context("reading OS-9 module-directory records")?;
    Ok(data
        .chunks_exact(16)
        .map(|entry| ModuleDirectoryEntry {
            module: u32::from_be_bytes(entry[0..4].try_into().unwrap()),
            group: u32::from_be_bytes(entry[4..8].try_into().unwrap()),
            group_size: u32::from_be_bytes(entry[8..12].try_into().unwrap()),
            links: u16::from_be_bytes(entry[12..14].try_into().unwrap()),
            checksum: u16::from_be_bytes(entry[14..16].try_into().unwrap()),
        })
        .collect())
}

/// The system ROM is an OS-9 module group rather than a fixed address.  The
/// group containing the most resident modules is the system group; transient
/// programs (including cdi_stub) normally occur as one-off groups.  This is
/// derived exclusively from F$GModDr, so it also follows a differently mapped
/// player ROM.
fn system_rom_group(entries: &[ModuleDirectoryEntry]) -> Option<(u32, u32)> {
    let mut groups: HashMap<(u32, u32), usize> = HashMap::new();
    for entry in entries {
        let Some(group_end) = entry.group.checked_add(entry.group_size) else {
            continue;
        };
        if entry.module != 0
            && entry.group != 0
            && entry.group_size != 0
            && entry.module >= entry.group
            && entry.module < group_end
        {
            *groups.entry((entry.group, entry.group_size)).or_default() += 1;
        }
    }
    groups
        .into_iter()
        .max_by_key(|((_, size), members)| (*members, *size))
        .map(|(group, _)| group)
}

const OS9_MODULE_HEADER_SIZE: usize = 48;
const OS9_INIT_MEMORY_LIST_OFFSET: usize = 0x6a;
const OS9_MEMORY_REGION_SIZE: usize = 32;

#[derive(Debug)]
struct Os9MemoryRegion {
    memory_type: u16,
    priority: i16,
    access: u16,
    start: u32,
    end: u32,
}

#[derive(Debug)]
struct Os9Module {
    address: u32,
    size: u32,
    owner: u32,
    access: u16,
    kind: u8,
    attributes: u8,
    revision: u8,
    edition: u16,
    crc: [u8; 3],
    links: u16,
    name: String,
}

fn read_memory<T: Read + Write>(
    session: &mut Session<T>,
    address: u32,
    size: usize,
) -> Result<Vec<u8>> {
    session
        .set_address(address)
        .with_context(|| format!("selecting CD-i memory at 0x{address:08X}"))?;
    session
        .read(size)
        .with_context(|| format!("reading {size} bytes at 0x{address:08X}"))
}

fn os9_name(bytes: &[u8]) -> String {
    let mut name = Vec::new();
    for &byte in bytes {
        if byte == 0 {
            break;
        }
        name.push(byte & 0x7f);
        if byte & 0x80 != 0 {
            break;
        }
    }
    String::from_utf8_lossy(&name).into_owned()
}

fn module_name<T: Read + Write>(session: &mut Session<T>, address: u32) -> Result<String> {
    let header = read_memory(session, address, OS9_MODULE_HEADER_SIZE)
        .with_context(|| format!("reading OS-9 module header at {address:08X}"))?;
    if header[0..2] != [0x4a, 0xfc] {
        bail!("invalid OS-9 module sync at {address:08X}");
    }
    let size = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let offset = u32::from_be_bytes(header[12..16].try_into().unwrap());
    if size < OS9_MODULE_HEADER_SIZE as u32 || offset >= size {
        bail!("invalid OS-9 module header at {address:08X}");
    }
    let name_address = address
        .checked_add(offset)
        .context("OS-9 module name address overflow")?;
    Ok(os9_name(&read_memory(
        session,
        name_address,
        (size - offset).min(64) as usize,
    )?))
}

fn memory_regions<T: Read + Write>(
    session: &mut Session<T>,
    entries: &[ModuleDirectoryEntry],
    verbose: bool,
) -> Result<(Vec<Os9MemoryRegion>, Vec<(u32, u32)>)> {
    let modules: Vec<_> = entries
        .iter()
        .filter(|entry| entry.module != 0)
        .copied()
        .collect();
    let mut init = None;
    let mut vmpeg_groups = Vec::new();
    progress_items("Inspecting module groups", 0, modules.len());
    for (index, entry) in modules.iter().enumerate() {
        let name = module_name(session, entry.module)?;
        if name == "init" {
            init = Some(entry.module);
        }
        if name.eq_ignore_ascii_case("vmpeg") {
            vmpeg_groups.push((entry.group, entry.group_size));
        }
        progress_items("Inspecting module groups", index + 1, modules.len());
    }
    eprintln!();
    let init = init.context("could not find the OS-9 init module in F$GModDr output")?;
    let offset_address = init
        .checked_add(OS9_INIT_MEMORY_LIST_OFFSET as u32)
        .context("OS-9 init memory-list offset overflow")?;
    let offset = u16::from_be_bytes(
        read_memory(session, offset_address, 2)?
            .try_into()
            .expect("two-byte read has exact length"),
    ) as u32;
    if offset == 0 {
        bail!("the OS-9 init module has no memory list");
    }
    let list_address = init
        .checked_add(offset)
        .context("OS-9 memory-list address overflow")?;
    let data = read_memory(session, list_address, 4096)?;
    let mut regions = Vec::new();
    for record in data.chunks_exact(OS9_MEMORY_REGION_SIZE) {
        let memory_type = u16::from_be_bytes(record[0..2].try_into().unwrap());
        let priority = i16::from_be_bytes(record[2..4].try_into().unwrap());
        if memory_type == 0 || priority == 0 {
            break;
        }
        let region = Os9MemoryRegion {
            memory_type,
            priority,
            access: u16::from_be_bytes(record[4..6].try_into().unwrap()),
            start: u32::from_be_bytes(record[8..12].try_into().unwrap()),
            end: u32::from_be_bytes(record[12..16].try_into().unwrap()),
        };
        if verbose {
            os9_trace(
                true,
                format!(
                    "memory region {:08X}-{:08X} type={:04X} priority={} access={:04X}",
                    region.start, region.end, region.memory_type, region.priority, region.access
                ),
            );
        }
        if region.end > region.start {
            regions.push(region);
        }
    }
    vmpeg_groups.sort_unstable();
    vmpeg_groups.dedup();
    Ok((regions, vmpeg_groups))
}

fn list_roms<T: Read + Write>(session: &mut Session<T>, verbose: bool) -> Result<()> {
    eprintln!("Reading module directory...");
    let entries = module_directory(session)?;
    eprintln!("Reading memory list...");
    let (memory, vmpeg_groups) = memory_regions(session, &entries, verbose)?;
    let system_group = system_rom_group(&entries);
    let mut groups: HashMap<(u32, u32), usize> = HashMap::new();
    for entry in &entries {
        let Some(end) = entry.group.checked_add(entry.group_size) else {
            continue;
        };
        if entry.module == 0
            || entry.group == 0
            || entry.group_size == 0
            || entry.module < entry.group
            || entry.module >= end
            // A group overlapping a configured OS-9 memory region is RAM,
            // video memory, or another writable area rather than ROM.
            || memory
                .iter()
                .any(|region| entry.group < region.end && end > region.start)
                    && !vmpeg_groups.contains(&(entry.group, entry.group_size))
        {
            continue;
        }
        *groups.entry((entry.group, entry.group_size)).or_default() += 1;
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_unstable_by_key(|((address, _), _)| *address);
    println!("  Addr     End       Size  ROM type     ROM description");
    println!("-------- -------- ------ ------------ ----------------");
    for (index, ((address, size), _)) in groups.iter().enumerate() {
        let is_system = Some((*address, *size)) == system_group;
        let is_vmpeg = vmpeg_groups.contains(&(*address, *size));
        let name = if is_system {
            "cdi000x.rom".to_owned()
        } else if is_vmpeg {
            "vmpeg.rom".to_owned()
        } else {
            format!("rom{index:03}x.rom")
        };
        println!(
            "{:08x} {:08x} {:>5}K {:<12} {}",
            address,
            address + size - 1,
            size / 1024,
            name,
            if is_system {
                "Unknown CD-i system ROM"
            } else if is_vmpeg {
                "VMPEG expansion ROM"
            } else {
                "Unknown ROM candidate"
            }
        );
    }
    if groups.is_empty() {
        eprintln!("No non-RAM module groups were reported.");
    }
    Ok(())
}

fn module_kind_name(kind: u8) -> String {
    match kind {
        1 => "Prog".into(),
        2 => "Subr".into(),
        4 => "Data".into(),
        11 => "Trap".into(),
        12 => "Sys".into(),
        13 => "Fman".into(),
        14 => "Driv".into(),
        15 => "Desc".into(),
        other => other.to_string(),
    }
}

fn read_module<T: Read + Write>(
    session: &mut Session<T>,
    entry: ModuleDirectoryEntry,
) -> Result<Os9Module> {
    let header = read_memory(session, entry.module, OS9_MODULE_HEADER_SIZE)
        .with_context(|| format!("reading OS-9 module header at {:08X}", entry.module))?;
    if header[0..2] != [0x4a, 0xfc] {
        bail!("invalid OS-9 module sync at {:08X}", entry.module);
    }
    let size = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let name_offset = u32::from_be_bytes(header[12..16].try_into().unwrap());
    if size < OS9_MODULE_HEADER_SIZE as u32 || name_offset >= size {
        bail!("invalid OS-9 module header at {:08X}", entry.module);
    }
    let name_address = entry
        .module
        .checked_add(name_offset)
        .context("OS-9 module name address overflow")?;
    let name_size = (size - name_offset).min(64) as usize;
    let name = os9_name(&read_memory(session, name_address, name_size)?);
    let crc_address = entry
        .module
        .checked_add(size - 3)
        .context("OS-9 module CRC address overflow")?;
    let crc: [u8; 3] = read_memory(session, crc_address, 3)?
        .try_into()
        .expect("three-byte read has exact length");
    Ok(Os9Module {
        address: entry.module,
        size,
        owner: u32::from_be_bytes(header[8..12].try_into().unwrap()),
        access: u16::from_be_bytes(header[16..18].try_into().unwrap()),
        kind: header[18],
        attributes: header[20],
        revision: header[21],
        edition: u16::from_be_bytes(header[22..24].try_into().unwrap()),
        crc,
        links: entry.links,
        name,
    })
}

fn list_modules<T: Read + Write>(session: &mut Session<T>, verbose: bool) -> Result<()> {
    eprintln!("Reading module directory...");
    let entries = module_directory(session)?;
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.module != 0)
        .collect();
    eprintln!("Reading module information...");
    let module_count = entries.len();
    let mut modules = Vec::with_capacity(module_count);
    progress_items("Reading module information", 0, module_count);
    for (index, entry) in entries.into_iter().enumerate() {
        if verbose {
            os9_trace(true, format!("module header {:08X}", entry.module));
        }
        modules.push(read_module(session, entry)?);
        progress_items("Reading module information", index + 1, module_count);
    }
    eprintln!();
    println!("Found {} modules", modules.len());
    println!("  Addr     Size      Owner    Perm Type Revs  Ed #  Crc    Lnk  Module name");
    println!("-------- -------- ----------- ---- ---- ---- ----- ------ ----- -------------");
    for module in modules {
        println!(
            "{:08x} {:8} {:5}.{:<5} {:04x} {:<4} {:02x}{:02x} {:5} {:02x}{:02x}{:02x} {:5} {}",
            module.address,
            module.size,
            module.owner >> 16,
            module.owner & 0xffff,
            module.access,
            module_kind_name(module.kind),
            module.attributes,
            module.revision,
            module.edition,
            module.crc[0],
            module.crc[1],
            module.crc[2],
            module.links,
            module.name,
        );
    }
    Ok(())
}

fn prepare_rom_read(
    session: &mut Session<Box<dyn serialport::SerialPort>>,
    wait: bool,
    upload_baud: Option<u32>,
    mister: bool,
) -> Result<()> {
    if wait {
        eprintln!("Waiting for full CD-i Stub...");
        let greeting = session
            .wait_for_stub(4096)
            .context("waiting for full CD-i Stub")?;
        let greeting = banner(&greeting);
        if greeting.trim().is_empty() {
            bail!("ROM download subset is active; ROM utilities require a full cdi_stub");
        }
        eprintln!("Stub active: {}", greeting.trim());
    }
    if let Some(baud) = upload_baud {
        let selected = session
            .negotiate_baud_rate(baud)
            .context("negotiating ROM-read baud rate")?;
        if selected == 0 {
            bail!("the running Stub does not support baud-rate switching");
        }
        if !mister {
            session
                .transport_mut()
                .set_baud_rate(selected)
                .with_context(|| format!("switching local serial port to {selected} baud"))?;
        }
        eprintln!("ROM-read baud rate: {selected}");
    }
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
            Command::Rom {
                command:
                    RomCommand::Dump {
                        upload_baud: Some(_),
                        ..
                    }
                    | RomCommand::Verify {
                        upload_baud: Some(_),
                        ..
                    },
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
            print_directory(&mut session, path, *read_size, cli.verbose)?;
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
            let data = get_file(&mut session, remote_path, *chunk_size, cli.verbose)?;
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
            put_file(&mut session, &data, remote_path, *chunk_size, cli.verbose)?;
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
            delete_file(&mut session, remote_path, cli.verbose)?;
            eprintln!("Deleted {remote_path}.");
        }
        Command::Rom { command } => match command {
            RomCommand::Dump {
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
                prepare_rom_read(&mut session, *wait, *upload_baud, cli.mister)?;
                progress_bar("Dumping ROM", 0, *size);
                let data = session
                    .upload_with_progress(*address, *size, *chunk_size, |done| {
                        progress_bar("Dumping ROM", done, *size)
                    })
                    .context("ROM dump failed")?;
                eprintln!();
                fs::write(file, &data).with_context(|| format!("writing ROM image to {file}"))?;
                if *end {
                    session.end().context("ending stub")?;
                }
                println!(
                    "ROM dump: {} bytes, CRC-32 {:08X}",
                    data.len(),
                    crc32_ieee(&data)
                );
            }
            RomCommand::Verify {
                file,
                address,
                chunk_size,
                upload_baud,
                wait,
                end,
            } => {
                let expected =
                    fs::read(file).with_context(|| format!("reading ROM image {file}"))?;
                if expected.is_empty() {
                    bail!("ROM image must not be empty");
                }
                if !(1..=u16::MAX as usize).contains(chunk_size) {
                    bail!("--chunk-size must be in 1..=65535");
                }
                prepare_rom_read(&mut session, *wait, *upload_baud, cli.mister)?;
                progress_bar("Verifying ROM", 0, expected.len());
                let actual = session
                    .upload_with_progress(*address, expected.len(), *chunk_size, |done| {
                        progress_bar("Verifying ROM", done, expected.len())
                    })
                    .context("ROM verification read failed")?;
                eprintln!();
                if *end {
                    session.end().context("ending stub")?;
                }
                if actual == expected {
                    println!(
                        "ROM verified: {} bytes, CRC-32 {:08X}",
                        actual.len(),
                        crc32_ieee(&actual)
                    );
                } else {
                    let mismatch = actual
                        .iter()
                        .zip(&expected)
                        .position(|(actual, expected)| actual != expected)
                        .unwrap_or(actual.len().min(expected.len()));
                    bail!(
                        "ROM mismatch at 0x{:08X}: player {:02X}, file {:02X} (player CRC-32 {:08X}, file CRC-32 {:08X})",
                        address.wrapping_add(mismatch as u32),
                        actual.get(mismatch).copied().unwrap_or(0),
                        expected.get(mismatch).copied().unwrap_or(0),
                        crc32_ieee(&actual),
                        crc32_ieee(&expected),
                    );
                }
            }
        },
        Command::RomList { wait, end } => {
            prepare_rom_read(&mut session, *wait, None, cli.mister)?;
            eprintln!("Building ROM list...");
            list_roms(&mut session, cli.verbose)?;
            if *end {
                session.end().context("ending stub")?;
            }
            println!("Done.");
        }
        Command::Mod { wait, end } => {
            prepare_rom_read(&mut session, *wait, None, cli.mister)?;
            list_modules(&mut session, cli.verbose)?;
            if *end {
                session.end().context("ending stub")?;
            }
            println!("Done.");
        }
        #[cfg(all(unix, feature = "fuse"))]
        Command::Mount { mountpoint } => {
            let fs = fuse::CdiFuse::new(session, cli.verbose);
            eprintln!(
                "Mounting CD-i filesystem at {mountpoint}; only new /nvr files are writable. Unmount with fusermount3 -u {mountpoint}."
            );
            fuse::mount(fs, mountpoint)?;
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
