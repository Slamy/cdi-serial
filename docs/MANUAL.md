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

The original CD-i Link executable remains an optional legacy way to load its
bundled `cdistub` and retain it with:

```sh
wine /path/to/cdilink.exe -port 5 -keep
```

Start or power on the player when CD-i Link waits for the Stub. On players
with the ROM download subset, CD-i Link downloads its bundled `cdistub`
automatically. `-keep` leaves that full Stub active after CD-i Link exits.

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

## Cross-compiling for ARM Linux

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
acknowledgements and retry on checksum rejection. It does not yet provide
automatic player discovery, full-Stub injection, ROM-location detection, or
OS-9 file copy.
