use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

pub fn open_read_only(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    file.read_at(buf, offset)
}

pub fn volume_of_file(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.dev())
}
