use failure::*;

use crate::tools;
use super::IndexFile;
use super::chunk_stat::*;
use super::chunk_store::*;

use std::sync::Arc;
use std::io::{Read, Write};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::os::unix::io::AsRawFd;
use uuid::Uuid;
use chrono::{Local, TimeZone};
use super::ChunkInfo;

/// Header format definition for fixed index files (`.fidx`)
#[repr(C)]
pub struct FixedIndexHeader {
    pub magic: [u8; 8],
    pub uuid: [u8; 16],
    pub ctime: u64,
    /// Sha256 over the index ``SHA256(digest1||digest2||...)``
    pub index_csum: [u8; 32],
    pub size: u64,
    pub chunk_size: u64,
    reserved: [u8; 4016], // overall size is one page (4096 bytes)
}

// split image into fixed size chunks

pub struct FixedIndexReader {
    store: Arc<ChunkStore>,
    _file: File,
    filename: PathBuf,
    pub chunk_size: usize,
    pub size: usize,
    index_length: usize,
    index: *mut u8,
    pub uuid: [u8; 16],
    pub ctime: u64,
    pub index_csum: [u8; 32],
}

// `index` is mmap()ed which cannot be thread-local so should be sendable
unsafe impl Send for FixedIndexReader {}

impl Drop for FixedIndexReader {

    fn drop(&mut self) {
        if let Err(err) = self.unmap() {
            eprintln!("Unable to unmap file {:?} - {}", self.filename, err);
        }
    }
}

impl FixedIndexReader {

    pub fn open(store: Arc<ChunkStore>, path: &Path) -> Result<Self, Error> {

        let full_path = store.relative_path(path);

        let mut file = File::open(&full_path)
            .map_err(|err| format_err!("Unable to open fixed index {:?} - {}", full_path, err))?;

        if let Err(err) = nix::fcntl::flock(file.as_raw_fd(), nix::fcntl::FlockArg::LockSharedNonblock) {
            bail!("unable to get shared lock on {:?} - {}", full_path, err);
        }

        let header_size = std::mem::size_of::<FixedIndexHeader>();

        // todo: use static assertion when available in rust
        if header_size != 4096 { bail!("got unexpected header size for {:?}", path); }

        let mut buffer = vec![0u8; header_size];
        file.read_exact(&mut buffer)?;

        let header = unsafe { &mut * (buffer.as_ptr() as *mut FixedIndexHeader) };

        if header.magic != super::FIXED_SIZED_CHUNK_INDEX_1_0 {
            bail!("got unknown magic number for {:?}", path);
        }

        let size = u64::from_le(header.size) as usize;
        let ctime = u64::from_le(header.ctime);
        let chunk_size = u64::from_le(header.chunk_size) as usize;

        let index_length = (size + chunk_size - 1)/chunk_size;
        let index_size = index_length*32;

        let rawfd = file.as_raw_fd();

        let stat = match nix::sys::stat::fstat(rawfd) {
            Ok(stat) => stat,
            Err(err) => bail!("fstat {:?} failed - {}", path, err),
        };

        let expected_index_size = (stat.st_size as usize) - header_size;
        if index_size != expected_index_size {
            bail!("got unexpected file size for {:?} ({} != {})",
                  path, index_size, expected_index_size);
        }

        let data = unsafe { nix::sys::mman::mmap(
            std::ptr::null_mut(),
            index_size,
            nix::sys::mman::ProtFlags::PROT_READ,
            nix::sys::mman::MapFlags::MAP_PRIVATE,
            file.as_raw_fd(),
            header_size as i64) }? as *mut u8;

        Ok(Self {
            store,
            filename: full_path,
            _file: file,
            chunk_size,
            size,
            index_length,
            index: data,
            ctime,
            uuid: header.uuid,
            index_csum: header.index_csum,
        })
    }

    fn unmap(&mut self) -> Result<(), Error> {

        if self.index == std::ptr::null_mut() { return Ok(()); }

        let index_size = self.index_length*32;

        if let Err(err) = unsafe { nix::sys::mman::munmap(self.index as *mut std::ffi::c_void, index_size) } {
            bail!("unmap file {:?} failed - {}", self.filename, err);
        }

        self.index = std::ptr::null_mut();

        Ok(())
    }

    pub fn print_info(&self) {
        println!("Filename: {:?}", self.filename);
        println!("Size: {}", self.size);
        println!("ChunkSize: {}", self.chunk_size);
        println!("CTime: {}", Local.timestamp(self.ctime as i64, 0).format("%c"));
        println!("UUID: {:?}", self.uuid);
    }
}

impl IndexFile for FixedIndexReader {
    fn index_count(&self) -> usize {
        self.index_length
    }

    fn index_digest(&self, pos: usize) -> Option<&[u8; 32]> {
        if pos >= self.index_length {
            None
        } else {
            Some(unsafe { std::mem::transmute(self.index.add(pos*32)) })
        }
    }

    fn index_bytes(&self) -> u64 {
        (self.index_length * self.chunk_size) as u64
    }
}

pub struct FixedIndexWriter {
    store: Arc<ChunkStore>,
    file: File,
    _lock: tools::ProcessLockSharedGuard,
    filename: PathBuf,
    tmp_filename: PathBuf,
    chunk_size: usize,
    size: usize,
    index_length: usize,
    index: *mut u8,
    pub uuid: [u8; 16],
    pub ctime: u64,
}

// `index` is mmap()ed which cannot be thread-local so should be sendable
unsafe impl Send for FixedIndexWriter {}

impl Drop for FixedIndexWriter {

    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp_filename); // ignore errors
        if let Err(err) = self.unmap() {
            eprintln!("Unable to unmap file {:?} - {}", self.tmp_filename, err);
        }
    }
}

impl FixedIndexWriter {

    pub fn create(store: Arc<ChunkStore>, path: &Path, size: usize, chunk_size: usize) -> Result<Self, Error> {

        let shared_lock = store.try_shared_lock()?;

        let full_path = store.relative_path(path);
        let mut tmp_path = full_path.clone();
        tmp_path.set_extension("tmp_fidx");

        let mut file = std::fs::OpenOptions::new()
            .create(true).truncate(true)
            .read(true)
            .write(true)
            .open(&tmp_path)?;

        let header_size = std::mem::size_of::<FixedIndexHeader>();

        // todo: use static assertion when available in rust
        if header_size != 4096 { panic!("got unexpected header size"); }

        let ctime = std::time::SystemTime::now().duration_since(
            std::time::SystemTime::UNIX_EPOCH)?.as_secs();

        let uuid = Uuid::new_v4();

        let buffer = vec![0u8; header_size];
        let header = unsafe { &mut * (buffer.as_ptr() as *mut FixedIndexHeader) };

        header.magic = super::FIXED_SIZED_CHUNK_INDEX_1_0;
        header.ctime = u64::to_le(ctime);
        header.size = u64::to_le(size as u64);
        header.chunk_size = u64::to_le(chunk_size as u64);
        header.uuid = *uuid.as_bytes();

        header.index_csum = [0u8; 32];

        file.write_all(&buffer)?;

        let index_length = (size + chunk_size - 1)/chunk_size;
        let index_size = index_length*32;
        nix::unistd::ftruncate(file.as_raw_fd(), (header_size + index_size) as i64)?;

        let data = unsafe { nix::sys::mman::mmap(
            std::ptr::null_mut(),
            index_size,
            nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
            nix::sys::mman::MapFlags::MAP_SHARED,
            file.as_raw_fd(),
            header_size as i64) }? as *mut u8;

        Ok(Self {
            store,
            file,
            _lock: shared_lock,
            filename: full_path,
            tmp_filename: tmp_path,
            chunk_size,
            size,
            index_length,
            index: data,
            ctime,
            uuid: *uuid.as_bytes(),
        })
    }

    pub fn index_length(&self) -> usize {
        self.index_length
    }

    fn unmap(&mut self) -> Result<(), Error> {

        if self.index == std::ptr::null_mut() { return Ok(()); }

        let index_size = self.index_length*32;

        if let Err(err) = unsafe { nix::sys::mman::munmap(self.index as *mut std::ffi::c_void, index_size) } {
            bail!("unmap file {:?} failed - {}", self.tmp_filename, err);
        }

        self.index = std::ptr::null_mut();

        Ok(())
    }

    pub fn close(&mut self)  -> Result<[u8; 32], Error> {

        if self.index == std::ptr::null_mut() { bail!("cannot close already closed index file."); }

        let index_size = self.index_length*32;
        let data = unsafe { std::slice::from_raw_parts(self.index, index_size) };
        let index_csum =  openssl::sha::sha256(data);

        self.unmap()?;

        use std::io::Seek;

        let csum_offset = proxmox::tools::offsetof!(FixedIndexHeader, index_csum);
        self.file.seek(std::io::SeekFrom::Start(csum_offset as u64))?;
        self.file.write_all(&index_csum)?;
        self.file.flush()?;

        if let Err(err) = std::fs::rename(&self.tmp_filename, &self.filename) {
            bail!("Atomic rename file {:?} failed - {}", self.filename, err);
        }

        Ok(index_csum)
    }

    // Note: We want to add data out of order, so do not assume any order here.
    pub fn add_chunk(&mut self, chunk_info: &ChunkInfo, stat: &mut ChunkStat) -> Result<(), Error> {

        let chunk_len = chunk_info.chunk_len as usize;
        let end = chunk_info.offset as usize;

        if end < chunk_len {
            bail!("got chunk with small offset ({} < {}", end, chunk_len);
        }

        let pos = end - chunk_len;

        if end > self.size {
            bail!("write chunk data exceeds size ({} >= {})", end, self.size);
        }

        // last chunk can be smaller
        if ((end != self.size) && (chunk_len != self.chunk_size)) ||
            (chunk_len > self.chunk_size) || (chunk_len == 0) {
                bail!("got chunk with wrong length ({} != {}", chunk_len, self.chunk_size);
            }

        if pos & (self.chunk_size-1) != 0 { bail!("add unaligned chunk (pos = {})", pos); }

        if (end as u64) != chunk_info.offset {
            bail!("got chunk with wrong offset ({} != {}", end, chunk_info.offset);
        }

        let (is_duplicate, compressed_size) = self.store.insert_chunk(&chunk_info.chunk)?;

        stat.chunk_count += 1;
        stat.compressed_size += compressed_size;

        let digest = chunk_info.chunk.digest();

        println!("ADD CHUNK {} {} {}% {} {}", pos, chunk_len,
                 (compressed_size*100)/(chunk_len as u64), is_duplicate, proxmox::tools::digest_to_hex(digest));

        if is_duplicate {
            stat.duplicate_chunks += 1;
        } else {
            stat.disk_size += compressed_size;
        }

        self.add_digest(pos / self.chunk_size, digest)
    }

    pub fn add_digest(&mut self, index: usize, digest: &[u8; 32]) -> Result<(), Error> {

        if index >= self.index_length {
            bail!("add digest failed - index out of range ({} >= {})", index, self.index_length);
        }

        if self.index == std::ptr::null_mut() { bail!("cannot write to closed index file."); }

        let index_pos = index*32;
        unsafe {
            let dst = self.index.add(index_pos);
            dst.copy_from_nonoverlapping(digest.as_ptr(), 32);
        }

        Ok(())
    }
}
