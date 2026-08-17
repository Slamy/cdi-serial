#![cfg(all(unix, feature = "fuse"))]

use super::*;
use crate::os9::{cdfm_entries, get_file, put_file, read_directory};
use std::{
    collections::HashMap,
    ffi::OsStr,
    sync::Mutex,
    time::{Duration, SystemTime},
};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};

const FUSE_TTL: Duration = Duration::from_secs(1);
fn mount_owner_uid() -> u32 {
    // FUSE reports these attributes to the host kernel. Make the mounted view
    // owned by the user who started cdi-serial so its writable /nvr directory
    // is usable without elevated privileges.
    unsafe { libc::geteuid() }
}

fn mount_owner_gid() -> u32 {
    unsafe { libc::getegid() }
}

struct PendingFile {
    path: String,
    data: Vec<u8>,
    committed: bool,
}

pub(crate) struct CdiFuse {
    session: Mutex<Session<Box<dyn serialport::SerialPort>>>,
    verbose: bool,
    paths: Mutex<HashMap<u64, String>>,
    directories: Mutex<HashMap<String, Vec<String>>>,
    sizes: Mutex<HashMap<String, u64>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    pending: Mutex<HashMap<u64, PendingFile>>,
}

pub(crate) fn mount(filesystem: CdiFuse, mountpoint: &str) -> Result<()> {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RW,
        MountOption::DefaultPermissions,
        MountOption::FSName("cdi-serial".into()),
    ];
    fuser::mount(filesystem, mountpoint, &config).context("mounting FUSE filesystem")
}

impl CdiFuse {
    pub(crate) fn new(session: Session<Box<dyn serialport::SerialPort>>, verbose: bool) -> Self {
        let mut paths = HashMap::new();
        paths.insert(1, "/".to_owned());
        Self {
            session: Mutex::new(session),
            verbose,
            paths: Mutex::new(paths),
            directories: Mutex::new(HashMap::new()),
            sizes: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn attr(ino: u64, directory: bool, size: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory { 0o755 } else { 0o644 },
            nlink: 1,
            uid: mount_owner_uid(),
            gid: mount_owner_gid(),
            rdev: 0,
            blksize: 256,
            flags: 0,
        }
    }
    fn path(&self, ino: u64) -> Option<String> {
        self.paths.lock().ok()?.get(&ino).cloned()
    }
    fn inode(&self, path: String) -> u64 {
        let mut paths = self.paths.lock().unwrap();
        if let Some((&ino, _)) = paths.iter().find(|(_, value)| **value == path) {
            return ino;
        }
        let ino = paths.len() as u64 + 2;
        paths.insert(ino, path);
        ino
    }
    fn names(&self, path: &str) -> std::result::Result<Vec<String>, ()> {
        if let Some(names) = self.directories.lock().unwrap().get(path).cloned() {
            return Ok(names);
        }
        let mut session = self.session.lock().map_err(|_| ())?;
        os9_trace(self.verbose, format!("FUSE readdir {path:?}"));
        let data = read_directory(&mut *session, path, 256, self.verbose).map_err(|_| ())?;
        let cdfm = cdfm_entries(&data);
        let names: Vec<String> = if !cdfm.is_empty() {
            let mut sizes = self.sizes.lock().unwrap();
            cdfm.into_iter()
                .filter_map(|(_, size, name)| {
                    (name != ".").then(|| {
                        sizes.insert(format!("{path}/{name}"), size as u64);
                        name
                    })
                })
                .collect()
        } else {
            data.chunks_exact(32)
                .filter_map(|entry| {
                    let end = entry[..28].iter().position(|&b| b == 0)?;
                    (end > 0).then(|| String::from_utf8_lossy(&entry[..end]).into_owned())
                })
                .collect()
        };
        self.directories
            .lock()
            .unwrap()
            .insert(path.to_owned(), names.clone());
        Ok(names)
    }
    fn size(&self, path: &str) -> Option<u64> {
        self.sizes.lock().ok()?.get(path).copied()
    }
    fn file(&self, path: &str) -> std::result::Result<Vec<u8>, ()> {
        if let Some(data) = self.files.lock().unwrap().get(path).cloned() {
            return Ok(data);
        }
        let mut session = self.session.lock().map_err(|_| ())?;
        os9_trace(self.verbose, format!("FUSE read {path:?}"));
        let data = get_file(&mut *session, path, 256, self.verbose).map_err(|_| ())?;
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), data.clone());
        Ok(data)
    }
    fn pending_data(&self, fh: u64) -> Option<Vec<u8>> {
        self.pending
            .lock()
            .ok()?
            .get(&fh)
            .map(|file| file.data.clone())
    }
    fn pending_handle(&self, fh: u64, ino: u64) -> Option<u64> {
        let pending = self.pending.lock().ok()?;
        if pending.contains_key(&fh) {
            Some(fh)
        } else {
            pending.contains_key(&ino).then_some(ino)
        }
    }
    fn commit_pending(&self, fh: u64) -> std::result::Result<(), ()> {
        let (path, data) = {
            let pending = self.pending.lock().map_err(|_| ())?;
            let file = pending.get(&fh).ok_or(())?;
            if file.committed {
                return Ok(());
            }
            (file.path.clone(), file.data.clone())
        };
        let mut session = self.session.lock().map_err(|_| ())?;
        os9_trace(self.verbose, format!("FUSE commit {path:?}"));
        put_file(&mut *session, &data, &path, 256, self.verbose).map_err(|_| ())?;
        self.pending
            .lock()
            .map_err(|_| ())?
            .get_mut(&fh)
            .ok_or(())?
            .committed = true;
        self.files
            .lock()
            .map_err(|_| ())?
            .insert(path.clone(), data.clone());
        self.sizes
            .lock()
            .map_err(|_| ())?
            .insert(path.clone(), data.len() as u64);
        if let Some(name) = path.rsplit('/').next() {
            let mut directories = self.directories.lock().map_err(|_| ())?;
            let names = directories.entry("/nvr".to_owned()).or_default();
            if !names.iter().any(|entry| entry == name) {
                names.push(name.to_owned());
            }
        }
        Ok(())
    }
}
impl Filesystem for CdiFuse {
    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_string_lossy();
        let parent_path = self.path(parent.0).unwrap_or_else(|| "/".into());
        let path = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };
        if parent_path == "/" && (name == "cd" || name == "nvr") {
            let ino = self.inode(path);
            reply.entry(&FUSE_TTL, &Self::attr(ino, true, 0), Generation(0));
            return;
        }
        match self.names(&parent_path) {
            Ok(names) if names.iter().any(|item| item == name.as_ref()) => {
                let ino = self.inode(path.clone());
                if let Some(data) = self.pending_data(ino) {
                    reply.entry(
                        &FUSE_TTL,
                        &Self::attr(ino, false, data.len() as u64),
                        Generation(0),
                    );
                } else if let Some(size) = self.size(&path) {
                    reply.entry(&FUSE_TTL, &Self::attr(ino, false, size), Generation(0));
                } else {
                    match self.file(&path) {
                        Ok(data) => reply.entry(
                            &FUSE_TTL,
                            &Self::attr(ino, false, data.len() as u64),
                            Generation(0),
                        ),
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
            }
            _ => reply.error(Errno::ENOENT),
        }
    }
    fn getattr(&self, _: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        let path = self.path(ino.0).unwrap_or_else(|| "/".into());
        if path == "/" || path == "/cd" || path == "/nvr" {
            reply.attr(&FUSE_TTL, &Self::attr(ino.0, true, 0));
        } else if let Some(data) = self.pending_data(ino.0) {
            reply.attr(&FUSE_TTL, &Self::attr(ino.0, false, data.len() as u64));
        } else if let Some(size) = self.size(&path) {
            reply.attr(&FUSE_TTL, &Self::attr(ino.0, false, size));
        } else {
            match self.file(&path) {
                Ok(data) => reply.attr(&FUSE_TTL, &Self::attr(ino.0, false, data.len() as u64)),
                Err(_) => reply.error(Errno::ENOENT),
            }
        }
    }
    fn open(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if let Some(handle) = self.pending_handle(ino.0, ino.0) {
            reply.opened(FileHandle(handle), FopenFlags::empty());
            return;
        }
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }
    fn read(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .pending_data(ino.0)
            .or_else(|| self.path(ino.0).and_then(|p| self.file(&p).ok()))
        {
            Some(data) => {
                let start = (offset as usize).min(data.len());
                let end = (start + size as usize).min(data.len());
                reply.data(&data[start..end]);
            }
            None => reply.error(Errno::ENOENT),
        }
    }
    fn create(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        _: u32,
        _: u32,
        _: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = self.path(parent.0).unwrap_or_else(|| "/".into());
        let name = name.to_string_lossy();
        if parent_path == "/cd" {
            reply.error(Errno::EROFS);
            return;
        }
        if parent_path != "/nvr" {
            reply.error(Errno::EACCES);
            return;
        }
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            reply.error(Errno::EINVAL);
            return;
        }
        if name.len() > 28 {
            reply.error(Errno::ENAMETOOLONG);
            return;
        }
        let path = format!("/nvr/{name}");
        match self.names("/nvr") {
            Ok(names) if names.iter().any(|entry| entry == name.as_ref()) => {
                reply.error(Errno::EEXIST);
            }
            Err(_) => reply.error(Errno::EIO),
            Ok(_) => {
                let ino = self.inode(path.clone());
                self.pending.lock().unwrap().insert(
                    ino,
                    PendingFile {
                        path,
                        data: Vec::new(),
                        committed: false,
                    },
                );
                reply.created(
                    &FUSE_TTL,
                    &Self::attr(ino, false, 0),
                    Generation(0),
                    FileHandle(ino),
                    FopenFlags::empty(),
                );
            }
        }
    }
    fn write(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Ok(offset) = usize::try_from(offset) else {
            reply.error(Errno::EFBIG);
            return;
        };
        let Some(end) = offset.checked_add(data.len()) else {
            reply.error(Errno::EFBIG);
            return;
        };
        let Some(handle) = self.pending_handle(fh.0, ino.0) else {
            reply.error(Errno::EROFS);
            return;
        };
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let Some(file) = pending.get_mut(&handle) else {
            reply.error(Errno::EROFS);
            return;
        };
        if file.committed {
            reply.error(Errno::EROFS);
            return;
        }
        if file.data.len() < end {
            file.data.resize(end, 0);
        }
        file.data[offset..end].copy_from_slice(data);
        reply.written(data.len() as u32);
    }
    fn flush(&self, _: &Request, _: INodeNo, _: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        // FUSE may issue flush before its final buffered WRITE request. The
        // CD-i file service can create a file only once, so defer its one-shot
        // I$Create/I$Write transaction until release (the final close).
        reply.ok();
    }
    fn release(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.pending_handle(fh.0, ino.0);
        let result = handle
            .map(|handle| self.commit_pending(handle))
            .unwrap_or(Ok(()));
        if let Some(handle) = handle {
            self.pending
                .lock()
                .ok()
                .and_then(|mut files| files.remove(&handle));
        }
        match result {
            Ok(()) => reply.ok(),
            Err(()) => reply.error(Errno::EIO),
        }
    }
    fn unlink(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = self.path(parent.0).unwrap_or_else(|| "/".into());
        let name = name.to_string_lossy();
        if parent_path == "/cd" {
            reply.error(Errno::EROFS);
            return;
        }
        if parent_path != "/nvr" {
            reply.error(Errno::EACCES);
            return;
        }
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            reply.error(Errno::EINVAL);
            return;
        }
        let path = format!("/nvr/{name}");
        if self
            .pending
            .lock()
            .ok()
            .is_some_and(|pending| pending.values().any(|file| file.path == path))
        {
            reply.error(Errno::EBUSY);
            return;
        }
        let result = self.session.lock().map_err(|_| ()).and_then(|mut session| {
            os9_trace(self.verbose, format!("FUSE unlink {path:?}"));
            delete_file(&mut *session, &path, self.verbose).map_err(|_| ())
        });
        match result {
            Ok(()) => {
                self.files
                    .lock()
                    .ok()
                    .and_then(|mut files| files.remove(&path));
                self.sizes
                    .lock()
                    .ok()
                    .and_then(|mut sizes| sizes.remove(&path));
                if let Ok(mut directories) = self.directories.lock() {
                    if let Some(names) = directories.get_mut("/nvr") {
                        names.retain(|entry| entry != name.as_ref());
                    }
                }
                reply.ok();
            }
            Err(()) => reply.error(Errno::EIO),
        }
    }
    fn readdir(
        &self,
        _: &Request,
        ino: INodeNo,
        _: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let path = self.path(ino.0).unwrap_or_else(|| "/".into());
        let names = if path == "/" {
            Ok(vec!["cd".into(), "nvr".into()])
        } else {
            self.names(&path)
        };
        match names {
            Ok(names) => {
                let mut entries = vec![
                    (ino.0, FileType::Directory, ".".to_owned()),
                    (1, FileType::Directory, "..".to_owned()),
                ];
                entries.extend(names.into_iter().map(|name| {
                    let child = if path == "/" {
                        format!("/{name}")
                    } else {
                        format!("{path}/{name}")
                    };
                    let kind = if path == "/" {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    (self.inode(child), kind, name)
                }));
                for (index, (child, kind, name)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(INodeNo(child), (index + 1) as u64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }
}
