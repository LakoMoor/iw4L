//! The browser filesystem: a case-insensitive in-memory tree.
//!
//! Shards are mounted once at startup (their bytes are already in page memory),
//! files are decoded chunk by chunk on read, and files the engine writes are
//! plain memory buffers.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use web_time::SystemTime;

use iw4pk::{Entry, Pack};

pub use crate::ArchiveEntry;

type Shard = Arc<Pack<Arc<Vec<u8>>>>;

#[derive(Clone)]
enum Data {
    Mem(Arc<Vec<u8>>),
    Packed { pack: Shard, entry: Arc<Entry> },
    Archive(Arc<Vec<ArchiveMember>>),
}

#[derive(Clone)]
struct ArchiveMember {
    name: String,
    pack: Shard,
    entry: Arc<Entry>,
}

#[derive(Clone)]
enum Node {
    Dir(Dir),
    File(Data),
}

#[derive(Clone, Default)]
struct Dir(BTreeMap<String, (String, Node)>);

fn tree() -> &'static Mutex<Dir> {
    static TREE: OnceLock<Mutex<Dir>> = OnceLock::new();
    TREE.get_or_init(|| Mutex::new(Dir::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Dir> {
    tree()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn parts(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }
    out
}

fn split_slashes(path: &Path) -> Vec<String> {
    // `a\b` style paths show up in engine strings; treat both separators alike.
    let text = path.to_string_lossy().replace('\\', "/");
    parts(Path::new(&text))
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: not found", path.display()),
    )
}

impl Dir {
    fn get(&self, comps: &[String]) -> Option<&Node> {
        let mut dir = self;
        for (i, c) in comps.iter().enumerate() {
            let (_, node) = dir.0.get(&c.to_lowercase())?;
            if i + 1 == comps.len() {
                return Some(node);
            }
            match node {
                Node::Dir(d) => dir = d,
                Node::File(_) => return None,
            }
        }
        None
    }

    fn dir_mut(&mut self, comps: &[String]) -> io::Result<&mut Dir> {
        let mut dir = self;
        for c in comps {
            let key = c.to_lowercase();
            let entry = dir
                .0
                .entry(key)
                .or_insert_with(|| (c.clone(), Node::Dir(Dir::default())));
            match &mut entry.1 {
                Node::Dir(d) => dir = d,
                Node::File(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a file is in the way",
                    ));
                }
            }
        }
        Ok(dir)
    }

    fn insert(&mut self, comps: &[String], node: Node) -> io::Result<()> {
        let (name, parent) = comps
            .split_last()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty path"))?;
        let dir = self.dir_mut(parent)?;
        dir.0.insert(name.to_lowercase(), (name.clone(), node));
        Ok(())
    }

    fn remove(&mut self, comps: &[String]) -> Option<Node> {
        let (name, parent) = comps.split_last()?;
        let mut dir = self;
        for c in parent {
            match &mut dir.0.get_mut(&c.to_lowercase())?.1 {
                Node::Dir(d) => dir = d,
                Node::File(_) => return None,
            }
        }
        dir.0.remove(&name.to_lowercase()).map(|(_, n)| n)
    }
}

fn data_len(data: &Data) -> u64 {
    match data {
        Data::Mem(v) => v.len() as u64,
        Data::Packed { entry, .. } => entry.raw_len,
        Data::Archive(members) => members.iter().map(|m| m.entry.raw_len).sum(),
    }
}

/// Mount `bytes` (one IW4PK shard) with its entry paths rooted at `root`.
/// Entries inside an `.iwd` (`main/iw_00.iwd/images/x.iwi`) become members of
/// an archive file `main/iw_00.iwd`.
pub fn mount_shard(root: impl AsRef<Path>, bytes: Vec<u8>) -> Result<usize, String> {
    let pack: Shard = Arc::new(Pack::open(Arc::new(bytes)).map_err(|e| e.to_string())?);
    let root = split_slashes(root.as_ref());
    let mut tree = lock();
    let mut archives: BTreeMap<Vec<String>, Vec<ArchiveMember>> = BTreeMap::new();
    let mut count = 0;
    for entry in &pack.index.entries {
        let entry_arc = Arc::new(entry.clone());
        let mut comps = root.clone();
        let rel: Vec<String> = entry.path.split('/').map(str::to_owned).collect();
        if let Some(at) = rel.iter().position(|p| p.to_ascii_lowercase().ends_with(".iwd")) {
            comps.extend(rel[..=at].iter().cloned());
            archives.entry(comps).or_default().push(ArchiveMember {
                name: rel[at + 1..].join("/"),
                pack: Arc::clone(&pack),
                entry: entry_arc,
            });
        } else {
            comps.extend(rel);
            tree.insert(
                &comps,
                Node::File(Data::Packed {
                    pack: Arc::clone(&pack),
                    entry: entry_arc,
                }),
            )
            .map_err(|e| e.to_string())?;
        }
        count += 1;
    }
    for (comps, mut members) in archives {
        // A later shard extends an archive an earlier one already mounted.
        if let Some(Node::File(Data::Archive(existing))) = tree.get(&comps) {
            let mut all: Vec<ArchiveMember> = existing.iter().cloned().collect();
            all.append(&mut members);
            members = all;
        }
        tree.insert(&comps, Node::File(Data::Archive(Arc::new(members))))
            .map_err(|e| e.to_string())?;
    }
    Ok(count)
}

pub fn archive_entries(archive: &Path) -> Option<Vec<ArchiveEntry>> {
    let tree = lock();
    match tree.get(&split_slashes(archive))? {
        Node::File(Data::Archive(m)) => Some(
            m.iter()
                .map(|m| ArchiveEntry {
                    name: m.name.clone(),
                    size: m.entry.raw_len,
                })
                .collect(),
        ),
        _ => None,
    }
}

pub fn archive_read(
    archive: &Path,
    entry: &str,
    limit: Option<usize>,
) -> Option<io::Result<Vec<u8>>> {
    let member = {
        let tree = lock();
        let Node::File(Data::Archive(m)) = tree.get(&split_slashes(archive))? else {
            return None;
        };
        m.iter()
            .find(|m| m.name.eq_ignore_ascii_case(entry))
            .cloned()?
    };
    let result = match limit {
        Some(n) => member.pack.read_prefix(&member.entry.path, n),
        None => member.pack.read(&member.entry.path),
    };
    Some(result.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())))
}

// ---- std::fs surface -------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileType {
    dir: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn is_file(&self) -> bool {
        !self.dir
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    len: u64,
    dir: bool,
}

impl Metadata {
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn is_file(&self) -> bool {
        !self.dir
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
    pub fn file_type(&self) -> FileType {
        FileType { dir: self.dir }
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::UNIX_EPOCH)
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::UNIX_EPOCH)
    }
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let path = path.as_ref();
    let comps = split_slashes(path);
    if comps.is_empty() {
        return Ok(Metadata { len: 0, dir: true });
    }
    match lock().get(&comps) {
        Some(Node::Dir(_)) => Ok(Metadata { len: 0, dir: true }),
        Some(Node::File(d)) => Ok(Metadata {
            len: data_len(d),
            dir: false,
        }),
        None => Err(not_found(path)),
    }
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    metadata(path)
}

pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    metadata(path)?;
    Ok(PathBuf::from("/").join(split_slashes(path).join("/")))
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|m| m.dir)
}
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|m| !m.dir)
}

#[derive(Debug)]
pub struct DirEntry {
    parent: PathBuf,
    name: String,
    meta: Metadata,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.parent.join(&self.name)
    }
    pub fn file_name(&self) -> std::ffi::OsString {
        self.name.clone().into()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(self.meta.clone())
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.meta.file_type())
    }
}

pub struct ReadDir(std::vec::IntoIter<DirEntry>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Ok)
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref();
    let comps = split_slashes(path);
    let tree = lock();
    let dir = if comps.is_empty() {
        &*tree
    } else {
        match tree.get(&comps) {
            Some(Node::Dir(d)) => d,
            Some(Node::File(_)) => {
                return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a directory"));
            }
            None => return Err(not_found(path)),
        }
    };
    let entries: Vec<DirEntry> = dir
        .0
        .values()
        .map(|(name, node)| DirEntry {
            parent: path.to_path_buf(),
            name: name.clone(),
            meta: match node {
                Node::Dir(_) => Metadata { len: 0, dir: true },
                Node::File(d) => Metadata {
                    len: data_len(d),
                    dir: false,
                },
            },
        })
        .collect();
    Ok(ReadDir(entries.into_iter()))
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    lock().dir_mut(&split_slashes(path.as_ref())).map(|_| ())
}
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    create_dir_all(path)
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    lock()
        .remove(&split_slashes(path.as_ref()))
        .map(|_| ())
        .ok_or_else(|| not_found(path.as_ref()))
}
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    remove_file(path)
}
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    remove_file(path)
}

/// Files the engine wrote (not mounted shards) under `prefix`, for the page to keep.
pub fn export_written(prefix: impl AsRef<Path>) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Dir, here: &mut Vec<String>, out: &mut Vec<(String, Vec<u8>)>) {
        for (name, node) in dir.0.values() {
            here.push(name.clone());
            match node {
                Node::Dir(d) => walk(d, here, out),
                Node::File(Data::Mem(bytes)) => out.push((here.join("/"), bytes.as_ref().clone())),
                Node::File(_) => {}
            }
            here.pop();
        }
    }
    let comps = split_slashes(prefix.as_ref());
    let tree = lock();
    let mut out = Vec::new();
    if let Some(Node::Dir(dir)) = tree.get(&comps) {
        let mut here = comps.clone();
        walk(dir, &mut here, &mut out);
    }
    out
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let mut tree = lock();
    let node = tree
        .remove(&split_slashes(from.as_ref()))
        .ok_or_else(|| not_found(from.as_ref()))?;
    tree.insert(&split_slashes(to.as_ref()), node)
}

pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let bytes = read(from)?;
    write(to, &bytes)?;
    Ok(bytes.len() as u64)
}

fn read_all(data: &Data) -> io::Result<Vec<u8>> {
    match data {
        Data::Mem(v) => Ok(v.as_ref().clone()),
        Data::Packed { pack, entry } => pack
            .read(&entry.path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        Data::Archive(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an archive is opened through vfs::archive_read",
        )),
    }
}

fn data_of(path: &Path) -> io::Result<Data> {
    match lock().get(&split_slashes(path)) {
        Some(Node::File(d)) => Ok(d.clone()),
        Some(Node::Dir(_)) => Err(io::Error::new(io::ErrorKind::IsADirectory, "is a directory")),
        None => Err(not_found(path)),
    }
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    read_all(&data_of(path.as_ref())?)
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"))
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    lock().insert(
        &split_slashes(path.as_ref()),
        Node::File(Data::Mem(Arc::new(contents.as_ref().to_vec()))),
    )
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    write: bool,
    create: bool,
    truncate: bool,
    append: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, _on: bool) -> &mut Self {
        self
    }
    pub fn write(&mut self, on: bool) -> &mut Self {
        self.write = on;
        self
    }
    pub fn create(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }
    pub fn create_new(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }
    pub fn truncate(&mut self, on: bool) -> &mut Self {
        self.truncate = on;
        self
    }
    pub fn append(&mut self, on: bool) -> &mut Self {
        self.append = on;
        self.write = on || self.write;
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        if !self.write && !self.append {
            return File::open(path);
        }
        let existing = match data_of(path) {
            Ok(d) if !self.truncate => read_all(&d)?,
            Ok(_) => Vec::new(),
            Err(e) if self.create && e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let pos = if self.append { existing.len() as u64 } else { 0 };
        let mut file = File {
            path: path.to_path_buf(),
            body: Body::Buffer(existing),
            pos,
            dirty: false,
        };
        if self.create || self.truncate {
            file.commit();
        }
        Ok(file)
    }
}

enum Body {
    Buffer(Vec<u8>),
    Packed {
        pack: Shard,
        entry: Arc<Entry>,
        cache: Option<(usize, Vec<u8>)>,
    },
}

pub struct File {
    path: PathBuf,
    body: Body,
    pos: u64,
    dirty: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        let body = match data_of(path)? {
            Data::Mem(v) => Body::Buffer(v.as_ref().clone()),
            Data::Packed { pack, entry } => Body::Packed {
                pack,
                entry,
                cache: None,
            },
            Data::Archive(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "archives are read through vfs::archive_read",
                ));
            }
        };
        Ok(File {
            path: path.to_path_buf(),
            body,
            pos: 0,
            dirty: false,
        })
    }

    pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(Metadata {
            len: self.len(),
            dir: false,
        })
    }

    pub fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        if let Body::Buffer(b) = &mut self.body {
            b.resize(len as usize, 0);
            self.dirty = true;
        }
        Ok(())
    }
    pub fn try_clone(&self) -> io::Result<File> {
        File::open(&self.path)
    }

    fn len(&self) -> u64 {
        match &self.body {
            Body::Buffer(b) => b.len() as u64,
            Body::Packed { entry, .. } => entry.raw_len,
        }
    }

    fn commit(&mut self) {
        if let Body::Buffer(b) = &self.body {
            let _ = lock().insert(
                &split_slashes(&self.path),
                Node::File(Data::Mem(Arc::new(b.clone()))),
            );
            self.dirty = false;
        }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if self.dirty {
            self.commit();
        }
    }
}

impl Read for File {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len() {
            return Ok(0);
        }
        match &mut self.body {
            Body::Buffer(b) => {
                let start = self.pos as usize;
                let n = out.len().min(b.len() - start);
                out[..n].copy_from_slice(&b[start..start + n]);
                self.pos += n as u64;
                Ok(n)
            }
            Body::Packed { pack, entry, cache } => {
                // Find the chunk holding `pos`, decode it once, serve from it.
                let mut base = 0u64;
                let mut idx = 0;
                for (i, c) in entry.chunks.iter().enumerate() {
                    if self.pos < base + u64::from(c.raw_len) {
                        idx = i;
                        break;
                    }
                    base += u64::from(c.raw_len);
                }
                if cache.as_ref().is_none_or(|(i, _)| *i != idx) {
                    let data = pack
                        .read_chunk(entry, idx)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                    *cache = Some((idx, data));
                }
                let (_, data) = cache.as_ref().unwrap();
                let off = (self.pos - base) as usize;
                let n = out.len().min(data.len() - off);
                out[..n].copy_from_slice(&data[off..off + n]);
                self.pos += n as u64;
                Ok(n)
            }
        }
    }
}

impl Read for &File {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut clone = File::open(&self.path)?;
        clone.pos = self.pos;
        clone.read(out)
    }
}

impl Seek for File {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let len = self.len() as i128;
        let next = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(d) => len + i128::from(d),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
        };
        if next < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Body::Buffer(b) = &mut self.body else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "packed files are read-only",
            ));
        };
        let start = self.pos as usize;
        if b.len() < start {
            b.resize(start, 0);
        }
        let overlap = (b.len() - start).min(buf.len());
        b[start..start + overlap].copy_from_slice(&buf[..overlap]);
        b.extend_from_slice(&buf[overlap..]);
        self.pos += buf.len() as u64;
        self.dirty = true;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.dirty {
            self.commit();
        }
        Ok(())
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File").field("path", &self.path).finish_non_exhaustive()
    }
}
