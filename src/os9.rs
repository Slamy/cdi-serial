use super::*;

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

pub(crate) fn cdfm_entries(data: &[u8]) -> Vec<(u32, u32, String)> {
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

pub(crate) fn print_directory_entries(data: &[u8]) -> usize {
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

pub(crate) fn read_directory<T: Read + Write>(
    session: &mut Session<T>,
    path: &str,
    read_size: usize,
    trace: bool,
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
    os9_trace(trace, format!("I$Open {path:?} (directory)"));
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
    os9_trace(trace, format!("I$GetStt {path:?}"));
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
            os9_trace(
                trace,
                format!("I$Read {path:?} ({read_size} bytes requested)"),
            );
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
    os9_trace(trace, format!("I$Close {path:?}"));
    let close_result = session.os9_call(OS9_I_CLOSE, 0);
    let directory_data = result?;
    close_result.context("closing OS-9 directory")?;
    Ok(directory_data)
}

pub(crate) fn print_directory<T: Read + Write>(
    session: &mut Session<T>,
    path: &str,
    read_size: usize,
    trace: bool,
) -> Result<()> {
    let directory_data = read_directory(session, path, read_size, trace)?;
    if print_directory_entries(&directory_data) == 0 {
        eprintln!("Directory is empty.");
    }
    eprintln!("Done.");
    Ok(())
}

pub(crate) fn get_file<T: Read + Write>(
    session: &mut Session<T>,
    remote_path: &str,
    chunk_size: usize,
    trace: bool,
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
    os9_trace(trace, format!("I$Open {remote_path:?} (read)"));
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
            os9_trace(
                trace,
                format!("I$Read {remote_path:?} ({chunk_size} bytes requested)"),
            );
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
    os9_trace(trace, format!("I$Close {remote_path:?}"));
    let close_result = session.os9_call(OS9_I_CLOSE, 0);
    let data = result?;
    close_result.context("closing OS-9 source file")?;
    Ok(data)
}

pub(crate) fn put_file<T: Read + Write>(
    session: &mut Session<T>,
    local_data: &[u8],
    remote_path: &str,
    chunk_size: usize,
    trace: bool,
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
    os9_trace(trace, format!("I$Create {remote_path:?}"));
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
            os9_trace(
                trace,
                format!("I$Write {remote_path:?} ({} bytes)", chunk.len()),
            );
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
    os9_trace(trace, format!("I$Close {remote_path:?}"));
    let close_result = session.os9_call(OS9_I_CLOSE, 0);
    result?;
    close_result.context("closing OS-9 destination file")?;
    Ok(())
}

pub(crate) fn delete_file<T: Read + Write>(
    session: &mut Session<T>,
    remote_path: &str,
    trace: bool,
) -> Result<()> {
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
    os9_trace(trace, format!("I$Delete {remote_path:?}"));
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
