# cdilink-rs

`cdilink-rs` is a Rust command-line uploader for the CD-i **Stub** protocol
used by CD-i Link. It uploads an image into CD-i memory, and can start it or
finish the stub session.

The protocol implementation is based on the public `cdistub-0.5.1` protocol
definition: requests start with `SOH`, use big-endian sizes/addresses, carry an
XOR check byte, and are retried after `NAK`.

## Build

```sh
cargo build --release
```

## Upload an application

For a development image that is loaded through the player's built-in download
subset, connect the CD-i null-modem cable and run:

```sh
./target/release/cdilink --port /dev/ttyUSB0 upload app.bin \
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

The client explicitly asserts DTR and RTS. The documented CD-i null-modem cable
feeds those host control lines into the player's CTS/RTS inputs, and some USB
serial adapters otherwise leave RTS low.

`--wait` waits for a full stub's `EM` activation notification; use it only when
attaching to a running full `cdi_stub`. `--execute` is a separate low-level
operation, and `--wait-for-return` is suitable only when that routine returns.

The direct-loader handshake starts at 9600 baud, then uses 19200 baud. The
default 256-byte writes match the working CD-i Link trace.

## Reading application debug output

Add `--terminal` to keep the serial connection open after the upload and copy
incoming serial bytes to standard output. This is the equivalent of CD-i Link's
`-terminal` mode and is useful with `--end`, once the application starts:

```sh
./target/release/cdilink --port /dev/ttyUSB0 upload app.bin \
  --address 8000 --end --reset --terminal
```

The terminal is receive-only and exits with Ctrl-C. By default it stays at the
19200 baud rate selected by the ROM download-subset handshake. If the
application emits diagnostics at another speed, add (for example)
`--terminal-baud 9600`.

To retain the raw terminal bytes as well as displaying them, add
`--terminal-log cdi-debug.log`. The log file is opened in append mode.

## Reading ROM or memory

`download` reads an address range from a **running full `cdi_stub`**. The
player's ROM download subset used by the direct application-loader workflow
does not implement the `READ` request, so `--reset` must not be used for this.
After starting or retaining a full stub, for example, read a 512 KiB ROM with:

```sh
./target/release/cdilink --port /dev/ttyUSB0 download cdi.rom \
  --address 400000 --size 524288 --wait
```

Addresses are hexadecimal. The destination file is created only after the
complete transfer succeeds; `--chunk-size` defaults to 256 bytes.

## Scope

This initial tool implements the documented transfer core: `ADDRESS`, `WRITE`,
`EXECUTE`, and `END`, with acknowledgements and retry on checksum rejection.
The ROM-only download subset is sufficient for the direct application-loader
workflow above; a full stub is only needed for `--wait` and future read/file
operations. Automatic discovery, bundled stub injection, ROM dumping, and OS-9
file copy are intentionally out of scope for this focused application uploader.
