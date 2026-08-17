# cdi-serial manual

## Installation

Build locally with `cargo build --release`, or install the executable into
Cargo's binary directory (usually `~/.cargo/bin`) with:

```sh
cargo install --path .
cdi-serial --help
```

On Linux, the user must be allowed to open the serial device. For a typical
USB adapter this means adding the user to `dialout`:

```sh
sudo usermod -aG dialout "$USER"
```

Log out and in again. Check a device's group with `ls -l /dev/ttyUSB0`; some
distributions use `uucp` instead.

## Transfer terminology

`download` always means **host to CD-i player**. `upload` always means **CD-i
player to host**. The protocol message named `READ` is used internally for an
upload.

## Downloading an application

For a development image loaded through the player's built-in download subset:

```sh
cdi-serial --port /dev/ttyUSB0 download app.bin --address 8000 --end --reset
```

Addresses are hexadecimal even without a prefix: `8000`, `0x8000`, and `$8000`
are equivalent. Choose the address required by the application's linker or
loader; `cdi-serial` does not guess a RAM address.

For an OS-9 `play` module, this is equivalent to CD-i Link's
`cdilink -n -a 8000 -d app -e`. `--reset` sends Ctrl-C in a separate
open/write/close cycle, waits for the download-subset marker, then transfers
the image. If the player has already been reset manually, use `--wait`.

The ROM download subset starts at 9600 baud, announces with `SOH` (`0x01`),
and changes to 19200 baud after the host replies with `ACK` (`0x06`). The
default 256-byte write size matches CD-i Link's working behavior. A full stub
instead announces activation with `EM` (`0x19`).

`--execute` is a separate low-level operation. For the direct OS-9 module
workflow, use `--end`, which hands the module to normal boot processing.

## Serial terminal and logging

Add `--terminal` after a download to keep the port open and display incoming
debug output until Ctrl-C:

```sh
cdi-serial --port /dev/ttyUSB0 download app.bin --address 8000 --end --reset --terminal
```

The terminal is receive-only. It normally stays at the post-bootstrap 19200
baud rate. Use `--terminal-baud 9600` to override it, or
`--terminal-log cdi-debug.log` to append the raw received bytes to a file.

## Uploading CD-i memory

The built-in download subset cannot service memory reads. Uploading a memory
range requires a running full `cdistub`.

### Starting a full Stub

The `cdistub` program is an OS-9 `play` module. Obtain it from the
[CD-i Stub distribution](https://www.cdiemu.org/site/cdilink.htm), then run:

```sh
cdi-serial --port /dev/ttyUSB0 stub /path/to/cdistub
```

`stub` resets the player, waits for the ROM download subset, downloads the
module to `0x8000`, and sends `END` so normal boot processing starts it. The
full Stub then remains active.

```sh
cdi-serial --port /dev/ttyUSB0 upload cdi.rom --address 400000 --size 524288 --upload-baud 19200
```

For a player without the ROM subset, boot the `cdi_stub` disc. Some models
need a model-specific Stub. Do not send Ctrl-C or use `--reset` after a full
Stub is active.

### Uploading a range

The full Stub normally begins at 9600 baud. To upload a 512 KiB ROM to the
host:

```sh
cdi-serial --port /dev/ttyUSB0 upload cdi.rom --address 400000 --size 524288
```

The destination file is written only after the full transfer succeeds. The
default read request size is 256 bytes.

Use `--upload-baud` to request a faster full-Stub transfer rate. The Stub
selects the highest supported rate at or below the requested value, then the
client changes its local serial rate to match:

```sh
cdi-serial --port /dev/ttyUSB0 upload cdi.rom --address 400000 --size 524288 --upload-baud 19200
```

Global `--baud` only sets the connection speed for an already-running Stub.
`--wait` is useful only if `cdi-serial` is started before a full Stub starts;
the full Stub's `EM` activation marker is sent once.

## ROM utilities

`rom dump` reads the standard CD-i system-ROM range (`0x400000`, 524,288 bytes)
and reports a CRC-32 checksum:

```sh
cdi-serial --port /dev/ttyUSB0 rom dump cdi.rom
```

`rom verify` reads the same number of bytes from the player and compares every
byte with a local dump. On mismatch it reports the first ROM address and both
bytes, plus CRC-32 values for both images:

```sh
cdi-serial --port /dev/ttyUSB0 rom verify cdi.rom
```

Both operations require a running full Stub. Use `--address`, `--size` (dump
only), `--chunk-size`, `--upload-baud`, `--wait`, and `--end` when needed.

### ROM list

`romlist` reads the OS-9 module directory and memory list to discover ROM
module groups. It reports the CD-i system ROM and, when present, the VMPEG
expansion ROM group:

```sh
cdi-serial --port /dev/ttyUSB0 romlist
```

It shows a module progress bar while inspecting headers. This is an OS-9
visible ROM inventory, not a universal physical-ROM probe: firmware that is
not represented by an OS-9 module cannot be discovered this way.

### Loaded module list

`mod` displays the active OS-9 modules directly from their live headers:
address, size, owner, permissions, type, revision, edition, CRC, link count,
and name.

```sh
cdi-serial --port /dev/ttyUSB0 mod
```

This command also shows a module progress bar while it reads headers, names,
and CRC values from the player.

## Listing OS-9 directories

`dir` is a read-only full-Stub operation. It opens an OS-9 directory and
prints each entry's name and file-descriptor address:

```sh
cdi-serial --port /dev/ttyUSB0 dir /nvr
```

Use `--wait` only when launching `cdi-serial` before the full Stub announces
itself. `--read-size` defaults to 256 bytes and must be a multiple of the
32-byte OS-9 directory-entry size.

## Copying files from the player

`get` reads an OS-9 file through the full Stub, so it does not need a memory
address or size. The local destination is written only after the transfer
succeeds:

```sh
cdi-serial --port /dev/ttyUSB0 get /cd/copyright copyright
```

Use `--chunk-size` to change the 256-byte default read size.

To copy a new host file to writable OS-9 storage, use `put`:

```sh
cdi-serial --port /dev/ttyUSB0 put settings.bin /nvr/settings.bin
```

The destination must not already exist. `put` uses OS-9 `I$Create` followed
by `I$Write`; it does not overwrite existing files.

To permanently delete one OS-9 file through the full Stub:

```sh
cdi-serial --port /dev/ttyUSB0 delete /nvr/old-settings.bin
```

`delete` requires an absolute path and cannot be undone. OS-9 rejects a file
that is open or not writable. An `E$FNA` (“file not accessible”) response
means OS-9 itself cannot open the directory entry; `delete` cannot repair or
remove such a damaged entry.

## Hardware integrity test

The opt-in integration test verifies a complete binary round trip: it writes a
1,280-byte payload to a new uniquely named `/nvr` file, reads it back, compares
every byte, then deletes that test file. It requires a running full Stub and is
ignored by default because it touches real hardware:

```sh
CDI_SERIAL_PORT=/dev/ttyUSB0 cargo test --test serial_integration -- --ignored
```

## FUSE mount (Linux)

The `mount` command is optional and is not included in a default build. With a
full Stub running, install your distribution's FUSE 3 package if needed (for
example, `sudo apt install fuse3`), then build with FUSE support:

```sh
cargo build --release --features fuse
```

Mount an existing empty directory:

```sh
mkdir -p /tmp/cdi-player
cdi-serial --port /dev/ttyUSB0 mount /tmp/cdi-player
```

Add `--verbose` before `mount` to print FUSE activity and each corresponding
OS-9 call (`I$Open`, `I$Read`, `I$Create`, `I$Write`, `I$Delete`, and
`I$Close`) to stderr:

```sh
cdi-serial --port /dev/ttyUSB0 --verbose mount /tmp/cdi-player
```

Keep that command running. In another terminal, read files normally:

```sh
ls /tmp/cdi-player/cd
cp /tmp/cdi-player/cd/copyright ./copyright
```

Unmount it when finished:

```sh
fusermount3 -u /tmp/cdi-player
```

Files are fetched on first access and cached for the lifetime of the mount.
New regular files may be written only in `/nvr`; their contents are staged
locally and sent to the player when the file is closed. `/cd`,
existing files, directories, and rename remain read-only or unsupported. Files
in `/nvr` can be permanently removed with ordinary `rm`; the same OS-9
limitations described for `delete` apply. Use `put` when you want an explicit
one-shot transfer instead.

## MiSTer FPGA

MiSTer's Linux serial connection is fixed at 115200 baud. Build with the
official toolchain as described in the
[MiSTer cross-compilation guide](https://mister-devel.github.io/MkDocs_MiSTer/developer/mistercompile/#general-prerequisites-for-arm-cross-compiling):

```sh
export PATH=/opt/gcc-arm-10.2-2020.11-x86_64-arm-none-linux-gnueabihf/bin:$PATH
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-none-linux-gnueabihf-gcc \
  cargo build --release --target armv7-unknown-linux-gnueabihf
scp target/armv7-unknown-linux-gnueabihf/release/cdi-serial root@mister:/media/fat
```

Use `--mister` to force 115200 baud for the entire session:

```sh
./cdi-serial --port /dev/ttyS1 --mister download app.bin --address 8000 --end --reset --terminal
```

MiSTer mode does not perform local protocol-directed baud changes. It cannot
be combined with `--terminal-baud` or `--upload-baud`.

## Cross-compiling

### Windows (x86_64)

From Debian, Ubuntu, or a similar Linux distribution, install the MinGW-w64
cross linker and Rust's 64-bit Windows GNU target:

```sh
sudo apt install gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --release --target x86_64-pc-windows-gnu
```

The executable is
`target/x86_64-pc-windows-gnu/release/cdi-serial.exe`. It supports the serial,
Stub, memory, ROM, module, and OS-9 file commands on Windows; the Linux/Unix
FUSE `mount` command is intentionally not included in the Windows build.

Use a Windows serial device name such as `COM3`:

```powershell
cdi-serial.exe --port COM3 romlist
```

### 32-bit ARMv7 (armv7l)

For a hard-float GNU/Linux target such as standard 32-bit Raspberry Pi OS:

```sh
rustup target add armv7-unknown-linux-gnueabihf
sudo apt install gcc-arm-linux-gnueabihf libc6-dev-armhf-cross
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
  cargo build --release --target armv7-unknown-linux-gnueabihf
```

Output: `target/armv7-unknown-linux-gnueabihf/release/cdi-serial`.
This target will not run on ARMv6-only devices.

### 64-bit ARM (aarch64)

For glibc-based 64-bit ARM Linux:

```sh
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu libc6-dev-arm64-cross
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
```

Output: `target/aarch64-unknown-linux-gnu/release/cdi-serial`.

These are dynamically linked GNU/Linux binaries; the target needs compatible
glibc. Rust distributes the target standard libraries through `rustup`; the
cross compiler and libc development package provide the target linker and
startup objects.

## Scope

The tool implements `ADDRESS`, `WRITE`, `READ`, `EXECUTE`, and `END`, including
acknowledgements and retry on checksum rejection. It supports full-Stub
bootstrap, ROM and module inspection, directory listing, OS-9 file copy, and
a FUSE view. It does not provide automatic player discovery or a universal
physical-ROM probe.
