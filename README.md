# cdi-serial

`cdi-serial` is a serial development tool for CD-i homebrew.

- `download` sends an application from the host to the CD-i player.
- `upload` reads a CD-i memory range into a file on the host.
- `--terminal` displays application debug output after a download.

The implementation is based on the public `cdistub-0.5.1` protocol and
interoperability testing against [CD-i Link](https://www.cdiemu.org/site/cdilink.htm).
The code was written with the AI tool Codex using `GPT 5.6 Terra`, then tested
and reviewed by human hand.

## Quick start

Build a release binary with a current stable [Rust toolchain](https://www.rust-lang.org/tools/install):

```sh
cargo build --release
```

Download an OS-9 `play` module to a player through its built-in download subset:

```sh
./target/release/cdi-serial --port /dev/ttyUSB0 download app.bin --address 8000 --end --reset
```

`8000` is hexadecimal, as in CD-i Link. Add `--terminal` to display serial
debug output after the application starts.

To upload a memory range from a running full `cdistub` to the host:

```sh
./target/release/cdi-serial --port /dev/ttyUSB0 upload cdi.rom --address 400000 --size 524288
```

Bootstrap a full Stub from its OS-9 module with `stub /path/to/cdistub`.
With that Stub running, list an OS-9 directory with `dir /nvr`.
Copy an OS-9 file from the player without finding its memory address or size:
`get /cd/copyright copyright`.
Copy a new host file to writable NVR storage with
`put settings.bin /nvr/settings.bin`.
On Linux, `mount /path/to/mountpoint` exposes a read-only FUSE view of `/cd`
and `/nvr` for ordinary tools such as `ls` and `cp`.

For installation, full-Stub setup, logging, FUSE mounting, MiSTer, ARM
cross-compilation, and troubleshooting, see the [manual](docs/MANUAL.md).

## Credits and references

This is an independent Rust implementation. It does not include or redistribute
CD-i Link, CD-i Stub, or any CD-i ROM image.

- **CD-i Stub 0.5.1 and the Stub protocol** — created by **CD-i Fan**.
  The protocol framing and `ADDRESS`, `WRITE`, `READ`, `EXECUTE`, and `END`
  message behavior were derived from the distribution's `stub/stubdefs.d` and
  `stub/stubcore.s` source files. The Stub source is distributed under the
  LGPL; see the package's licence files.
  [CD-i Stub / CD-i Link downloads](https://www.cdiemu.org/site/cdilink.htm)

- **CD-i Link 0.5.1** — created by **CD-i Fan**. Its `cdilink.txt` informed
  full-Stub bootstrap, `-keep`, terminal mode, and ROM-reading behavior.
  [CD-i Link project page](https://www.cdiemu.org/site/cdilink.htm)

- **Hardware interoperability observations** — based on a serial system-call
  trace of the original CD-i Link executable. It established the ROM
  download-subset bootstrap acknowledgement, 9600-to-19200 baud transition,
  256-byte write size, and request timing used by the direct-loader path.

Please use ROM dumps only where you are entitled to do so. CD-i system ROMs
remain copyrighted and are not part of this project.
