# cdi-serial

`cdi-serial` is a tool to assist CD-i homebrew developers

* Can upload an application to the machine for execution
* Can act as a output terminal after upload for reading debug prints
* Portable and written in Rust
* Is compatible with the full stub from cdilink for memory download

The code is written using the AI tool Codex with `GPT 5.6 Terra`. The functionality has been tested and reviewed by human hand.
The implementation is based on the public `cdistub-0.5.1` protocol and also
reverse engineering efforts of [cdilink.exe](https://www.cdiemu.org/site/cdilink.htm)

## Build

Install a current stable [Rust toolchain](https://www.rust-lang.org/tools/install),
then clone this repository and build a release binary:

```sh
cargo build --release
```

The executable is then `./target/release/cdi-serial`.

## Install

To install the executable into Cargo's binary directory (usually
`~/.cargo/bin`), run this from the repository root:

```sh
cargo install --path .
```

Ensure that `~/.cargo/bin` is in `PATH`, then verify the installation:

```sh
cdi-serial --help
```

## Uploading an application

For a development image that is loaded through the player's built-in download
subset, connect the CD-i null-modem cable and run:

```sh
./target/release/cdi-serial --port /dev/ttyUSB0 upload app.bin \
  --address 8000 --end --reset
```

Addresses follow original CD-i Link convention and are hexadecimal even without
a prefix: `8000`, `0x8000`, and `$8000` are equivalent. Use the address chosen
by your application's loader/linker setup. The tool deliberately does not guess
a safe RAM address: that varies between CD-i models and images.

For an OS-9 `play` module loaded through the player's built-in download subset,
the practical equivalent of `cdilink -n -a 8000 -d app -e` is the command above.
`--reset` sends the Ctrl-C byte using a separate open/write/close cycle, then
waits for the download subset activation notification before uploading—matching
the development script and CD-i Link's default `-wait` behavior. Do not use
`--execute`; `--end` hands the module to normal boot processing. If you have
already reset the player yourself, use `--wait` instead.

The player's ROM download subset starts with `SOH` (`0x01`); the client replies
with `ACK` (`0x06`) at 9600 baud and switches to 19200 baud before transfer. A
full `cdi_stub` signals with `EM` (`0x19`) instead.

`--wait` waits for a full stub's one-time `EM` activation notification; use it
only when this tool is already waiting before the stub starts. `--execute` is a
separate low-level operation, and `--wait-for-return` is suitable only when
that routine returns.

The direct-loader handshake starts at 9600 baud, then uses 19200 baud. The
default 256-byte writes match the working CD-i Link trace.

## Reading application debug output

Add `--terminal` to keep the serial connection open after the upload and copy
incoming serial bytes to standard output. This is the equivalent of CD-i Link's
`-terminal` mode and is useful with `--end`, once the application starts:

```sh
./target/release/cdi-serial --port /dev/ttyUSB0 upload app.bin \
  --address 8000 --end --reset --terminal
```

The terminal is receive-only and exits with Ctrl-C. By default it stays at the
19200 baud rate selected by the ROM download-subset handshake. If the
application emits diagnostics at another speed, add (for example)
`--terminal-baud 9600`.

To retain the raw terminal bytes as well as displaying them, add
`--terminal-log cdi-debug.log`. The log file is opened in append mode.

### Starting the full stub

The current tool deliberately does not yet install a full stub. If you have
the original CD-i Link executable, use it once to upload its bundled `cdistub`
and leave it running:

```sh
wine /path/to/cdilink.exe -port 5 -keep
```

Start or power on the player when CD-i Link says it is waiting for the stub.
On players that support the ROM download subset, CD-i Link uploads `cdistub`
automatically. `-keep` is important: it prevents CD-i Link from sending `END`,
so the full stub remains active after the program exits.

For a player without the ROM download subset, boot the `cdi_stub` disc instead.
Some models need a model-specific stub; consult the original CD-i Stub package
for the appropriate image. Do not send Ctrl-C or use this tool's `--reset`
after the full stub is running.

### Reading a memory range

The full stub normally begins at 9600 baud. Once it is active, read a 512 KiB
ROM with:

```sh
./target/release/cdi-serial --port /dev/ttyUSB0 download cdi.rom \
  --address 400000 --size 524288
```

Addresses are hexadecimal. The destination file is created only after the
complete transfer succeeds; `--chunk-size` defaults to 256 bytes.

To speed up a read, request a higher full-Stub transfer rate. The Stub chooses
the highest speed it supports that does not exceed the requested value, then
the client switches its serial port to the selected rate:

```sh
./target/release/cdi-serial --port /dev/ttyUSB0 download cdi.rom \
  --address 400000 --size 524288 --download-baud 19200
```

This is a negotiated switch, unlike the global `--baud` option, which only
sets the serial speed used to connect to an already-running Stub.

`--wait` is only for when this program is already running before a full stub
is started manually (for example, from a `cdi_stub` disc). A full stub emits
its activation marker once; CD-i Link consumes that marker while installing
the stub with `-keep`.

## Cross-compiling for 32-bit ARMv7 Linux (armv7l)

```sh
rustup target add armv7-unknown-linux-gnueabihf
sudo apt install gcc-arm-linux-gnueabihf libc6-dev-armhf-cross
```

Build a release executable:

```sh
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
  cargo build --release --target armv7-unknown-linux-gnueabihf
```

The output is:

```text
target/armv7-unknown-linux-gnueabihf/release/cdi-serial
```

## Cross-compiling for 64-bit ARM Linux (aarch64)

Install Rust's target standard library and a GNU aarch64 cross-linker. On
Debian or Ubuntu:

```sh
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu libc6-dev-arm64-cross
```

Build the release executable with the appropriate linker selected for this one
command:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
```

The resulting executable is:

```text
target/aarch64-unknown-linux-gnu/release/cdi-serial
```

You can then omit the environment variable:

```sh
cargo build --release --target aarch64-unknown-linux-gnu
```

## Credits and references

This is an independent Rust implementation. It does not include or redistribute
CD-i Link, CD-i Stub, or any CD-i ROM image.

- **CD-i Stub 0.5.1 and the Stub protocol** — created by **CD-i Fan**.
  The protocol framing and `ADDRESS`, `WRITE`, `READ`, `EXECUTE`, and `END`
  message behavior in this project were derived from the distribution's
  `stub/stubdefs.d` and `stub/stubcore.s` source files. The Stub source is
  distributed under the LGPL; see the package's licence files.
  [CD-i Stub / CD-i Link downloads](https://www.cdiemu.org/site/cdilink.htm)

- **CD-i Link 0.5.1** — created by **CD-i Fan**. Its accompanying
  `cdilink.txt` informed the command semantics, including full-stub bootstrap,
  `-keep`, terminal mode, and ROM-reading workflow.
  [CD-i Link project page](https://www.cdiemu.org/site/cdilink.htm)

- **Hardware interoperability observations** — based on a serial system-call
  trace of the original CD-i Link executable.
  That trace established the ROM download-subset bootstrap acknowledgement,
  9600-to-19200 baud transition, 256-byte write size, and request timing used
  by the direct-loader path.

Please use ROM dumps only where you are entitled to do so. CD-i system ROMs
remain copyrighted and are not part of this project.

## Scope

This initial tool implements the documented transfer core: `ADDRESS`, `WRITE`,
`READ`, `EXECUTE`, and `END`, with acknowledgements and retry on checksum
rejection.
The ROM-only download subset is sufficient for the direct application-loader
workflow above; a full stub is needed for memory reads. Automatic discovery,
bundled stub injection, ROM-location detection, and OS-9 file copy are
intentionally out of scope for this focused serial tool.

## Troubleshooting

### Problems with user rights

On Linux, your user must be allowed to open the serial device. For the common
`/dev/ttyUSB0` case this is usually the `dialout` group:

```sh
sudo usermod -aG dialout "$USER"
```
Log out and back in after changing group membership. Check the ownership and
group of a particular port with `ls -l /dev/ttyUSB0`; distributions may use a
different group such as `uucp`.
