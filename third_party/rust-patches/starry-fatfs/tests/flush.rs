use std::{
    io::{Cursor, Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

use fatfs::{format_volume, FileSystem, FormatVolumeOptions, FsOptions, StdIoWrapper};

#[derive(Clone)]
struct TrackingStorage {
    inner: Arc<Mutex<StorageState>>,
}

struct StorageState {
    cursor: Cursor<Vec<u8>>,
    flushes: usize,
}

impl TrackingStorage {
    fn new(size: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StorageState {
                cursor: Cursor::new(vec![0; size]),
                flushes: 0,
            })),
        }
    }

    fn flushes(&self) -> usize {
        self.inner.lock().unwrap().flushes
    }

    fn rewind(&self) {
        self.inner.lock().unwrap().cursor.set_position(0);
    }
}

impl Read for TrackingStorage {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().cursor.read(buf)
    }
}

impl Write for TrackingStorage {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().cursor.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().unwrap().flushes += 1;
        Ok(())
    }
}

impl Seek for TrackingStorage {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.lock().unwrap().cursor.seek(pos)
    }
}

#[test]
fn flush_is_non_consuming_and_reaches_storage() {
    let storage = TrackingStorage::new(4 * 1024 * 1024);
    let mut formatter = StdIoWrapper::new(storage.clone());
    format_volume(&mut formatter, FormatVolumeOptions::new()).unwrap();

    let baseline_flushes = storage.flushes();
    let fs = FileSystem::new(storage.clone(), FsOptions::new()).unwrap();
    {
        let root = fs.root_dir();
        let mut file = root.create_file("before.txt").unwrap();
        file.write_all(b"before flush").unwrap();
    }

    fs.flush().unwrap();
    assert!(storage.flushes() > baseline_flushes);

    let root = fs.root_dir();
    let mut file = root.create_file("after.txt").unwrap();
    file.write_all(b"filesystem remains mounted").unwrap();
}

#[test]
fn preextended_file_payload_survives_flush_and_remount() {
    const PAYLOAD: &[u8] = b"claim-released\n";

    let storage = TrackingStorage::new(4 * 1024 * 1024);
    let mut formatter = StdIoWrapper::new(storage.clone());
    format_volume(&mut formatter, FormatVolumeOptions::new()).unwrap();

    {
        let fs = FileSystem::new(storage.clone(), FsOptions::new()).unwrap();
        let root = fs.root_dir();
        let mut file = root.create_file("marker").unwrap();
        file.write_all(&vec![0; PAYLOAD.len()]).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(PAYLOAD).unwrap();
        file.flush().unwrap();
        drop(file);
        drop(root);
        fs.flush().unwrap();
    }

    storage.rewind();
    let fs = FileSystem::new(storage, FsOptions::new()).unwrap();
    let root = fs.root_dir();
    let mut file = root.open_file("marker").unwrap();
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, PAYLOAD);
}

#[test]
fn unmount_flushes_the_clean_volume_marker() {
    let storage = TrackingStorage::new(4 * 1024 * 1024);
    let mut formatter = StdIoWrapper::new(storage.clone());
    format_volume(&mut formatter, FormatVolumeOptions::new()).unwrap();

    let fs = FileSystem::new(storage.clone(), FsOptions::new()).unwrap();
    {
        let root = fs.root_dir();
        let mut file = root.create_file("dirty.txt").unwrap();
        file.write_all(b"dirty").unwrap();
    }
    let flushes_before_unmount = storage.flushes();

    fs.unmount().unwrap();
    assert!(storage.flushes() > flushes_before_unmount);
}
