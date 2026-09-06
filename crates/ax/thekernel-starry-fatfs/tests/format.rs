use std::io;
use std::io::prelude::*;
use std::sync::{Arc, Mutex};

use fatfs::{FatType, StdIoWrapper};
use fscommon::BufStream;

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const TEST_STR: &str = "Hi there Rust programmer!\n";

type FileSystem = fatfs::FileSystem<StdIoWrapper<BufStream<io::Cursor<Vec<u8>>>>>;

#[derive(Clone)]
struct FaultController(Arc<Mutex<FaultState>>);

struct FaultState {
    bytes: Vec<u8>,
    fail_write_at: Option<u64>,
}

struct FaultStorage {
    controller: FaultController,
    position: u64,
}

impl Clone for FaultStorage {
    fn clone(&self) -> Self {
        Self {
            controller: self.controller.clone(),
            position: 0,
        }
    }
}

impl FaultStorage {
    fn new(size: usize) -> Self {
        Self {
            controller: FaultController(Arc::new(Mutex::new(FaultState {
                bytes: vec![0; size],
                fail_write_at: None,
            }))),
            position: 0,
        }
    }

    fn fail_next_write_covering(&self, position: u64) {
        self.controller.0.lock().unwrap().fail_write_at = Some(position);
    }
}

impl Read for FaultStorage {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let state = self.controller.0.lock().unwrap();
        let start =
            usize::try_from(self.position).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "position"))?;
        let available = state.bytes.len().saturating_sub(start);
        let read = available.min(buf.len());
        buf[..read].copy_from_slice(&state.bytes[start..start + read]);
        self.position += read as u64;
        Ok(read)
    }
}

impl Write for FaultStorage {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.controller.0.lock().unwrap();
        let end = self.position.saturating_add(buf.len() as u64);
        if state
            .fail_write_at
            .is_some_and(|position| self.position <= position && position < end)
        {
            state.fail_write_at = None;
            return Err(io::Error::new(io::ErrorKind::Other, "injected write failure"));
        }
        let start =
            usize::try_from(self.position).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "position"))?;
        let end = start
            .checked_add(buf.len())
            .filter(|end| *end <= state.bytes.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WriteZero, "end of storage"))?;
        state.bytes[start..end].copy_from_slice(buf);
        self.position += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for FaultStorage {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        let len = self.controller.0.lock().unwrap().bytes.len() as i128;
        let current = i128::from(self.position);
        let next = match position {
            io::SeekFrom::Start(position) => i128::from(position),
            io::SeekFrom::End(offset) => len + i128::from(offset),
            io::SeekFrom::Current(offset) => current + i128::from(offset),
        };
        if !(0..=len).contains(&next) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek"));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

fn init_logger() {
    let _ = env_logger::builder().is_test(true).try_init();
}

fn format_fs(opts: fatfs::FormatVolumeOptions, total_bytes: u64) -> FileSystem {
    init_logger();
    // Init storage to 0xD1 bytes (value has been choosen to be parsed as normal file)
    let storage_vec: Vec<u8> = vec![0xD1_u8; total_bytes as usize];
    let storage_cur = io::Cursor::new(storage_vec);
    let mut buffered_stream = fatfs::StdIoWrapper::from(BufStream::new(storage_cur));
    fatfs::format_volume(&mut buffered_stream, opts).expect("format volume");
    fatfs::FileSystem::new(buffered_stream, fatfs::FsOptions::new()).expect("open fs")
}

fn basic_fs_test(fs: &FileSystem) {
    let stats = fs.stats().expect("stats");
    if fs.fat_type() == fatfs::FatType::Fat32 {
        // On FAT32 one cluster is allocated for root directory
        assert_eq!(stats.total_clusters(), stats.free_clusters() + 1);
    } else {
        assert_eq!(stats.total_clusters(), stats.free_clusters());
    }

    let root_dir = fs.root_dir();
    let entries = root_dir.iter().map(|r| r.unwrap()).collect::<Vec<_>>();
    assert_eq!(entries.len(), 0);

    let subdir1 = root_dir.create_dir("subdir1").expect("create_dir subdir1");
    let subdir2 = root_dir
        .create_dir("subdir1/subdir2 with long name")
        .expect("create_dir subdir2");

    let test_str = TEST_STR.repeat(1000);
    {
        let mut file = subdir2.create_file("test file name.txt").expect("create file");
        file.truncate().expect("truncate file");
        file.write_all(test_str.as_bytes()).expect("write file");
    }

    let mut file = root_dir
        .open_file("subdir1/subdir2 with long name/test file name.txt")
        .unwrap();
    let mut content = String::new();
    file.read_to_string(&mut content).expect("read_to_string");
    assert_eq!(content, test_str);

    let filenames = root_dir.iter().map(|r| r.unwrap().file_name()).collect::<Vec<String>>();
    assert_eq!(filenames, ["subdir1"]);

    let filenames = subdir2.iter().map(|r| r.unwrap().file_name()).collect::<Vec<String>>();
    assert_eq!(filenames, [".", "..", "test file name.txt"]);

    subdir1
        .rename("subdir2 with long name/test file name.txt", &root_dir, "new-name.txt")
        .expect("rename");

    let filenames = subdir2.iter().map(|r| r.unwrap().file_name()).collect::<Vec<String>>();
    assert_eq!(filenames, [".", ".."]);

    let filenames = root_dir.iter().map(|r| r.unwrap().file_name()).collect::<Vec<String>>();
    assert_eq!(filenames, ["subdir1", "new-name.txt"]);
}

fn test_format_fs(opts: fatfs::FormatVolumeOptions, total_bytes: u64) -> FileSystem {
    let fs = format_fs(opts, total_bytes);
    basic_fs_test(&fs);
    fs
}

#[test]
fn test_format_1mb() {
    let total_bytes = MB;
    let opts = fatfs::FormatVolumeOptions::new();
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat12);
}

#[test]
fn replace_rename_keeps_source_payload_and_reclaims_destination() {
    let fs = format_fs(fatfs::FormatVolumeOptions::new(), MB);
    let root = fs.root_dir();
    {
        let mut source = root.create_file("source.txt").expect("create source");
        source.write_all(b"source payload").expect("write source");
    }
    {
        let mut destination = root.create_file("destination.txt").expect("create destination");
        destination
            .write_all(b"obsolete destination payload")
            .expect("write destination");
    }
    let before = fs.stats().expect("stats before replace").free_clusters();

    root.rename_replace("source.txt", &root, "destination.txt")
        .expect("replace rename");

    assert!(root.open_file("source.txt").is_err());
    let mut destination = root.open_file("destination.txt").expect("open replacement");
    let mut payload = String::new();
    destination.read_to_string(&mut payload).expect("read replacement");
    assert_eq!(payload, "source payload");
    assert!(!fs.is_poisoned());
    assert!(fs.stats().expect("stats after replace").free_clusters() > before);
}

#[test]
fn replace_rename_rolls_back_when_source_deletion_fails() {
    let storage = FaultStorage::new(MB as usize);
    let mut formatter = StdIoWrapper::from(storage.clone());
    fatfs::format_volume(&mut formatter, fatfs::FormatVolumeOptions::new()).expect("format fault-injection volume");
    let fs = fatfs::FileSystem::new(storage.clone(), fatfs::FsOptions::new()).expect("open fault-injection volume");
    let root = fs.root_dir();
    {
        let mut source = root.create_file("source.txt").expect("create source");
        source.write_all(b"source payload").expect("write source");
    }
    {
        let mut destination = root.create_file("destination.txt").expect("create destination");
        destination
            .write_all(b"destination payload")
            .expect("write destination");
    }
    let source_position = root
        .iter()
        .map(|entry| entry.expect("directory entry"))
        .find(|entry| entry.eq_name("source.txt"))
        .expect("source entry")
        .entry_position();
    storage.fail_next_write_covering(source_position);

    assert!(root.rename_replace("source.txt", &root, "destination.txt").is_err());
    assert!(!fs.is_poisoned());

    let mut source = root.open_file("source.txt").expect("source restored");
    let mut source_payload = String::new();
    source
        .read_to_string(&mut source_payload)
        .expect("read restored source");
    assert_eq!(source_payload, "source payload");

    let mut destination = root.open_file("destination.txt").expect("destination restored");
    let mut destination_payload = String::new();
    destination
        .read_to_string(&mut destination_payload)
        .expect("read restored destination");
    assert_eq!(destination_payload, "destination payload");
}

#[test]
fn directory_rename_updates_dotdot_transactionally() {
    let fs = format_fs(fatfs::FormatVolumeOptions::new(), MB);
    let root = fs.root_dir();
    let source_parent = root.create_dir("source-parent").expect("source parent");
    let destination_parent = root.create_dir("destination-parent").expect("destination parent");
    source_parent.create_dir("child").expect("child directory");

    source_parent
        .rename("child", &destination_parent, "moved-child")
        .expect("move directory");

    assert!(source_parent.open_dir("child").is_err());
    let moved = destination_parent.open_dir("moved-child").expect("moved directory");
    let parent_names = moved
        .open_dir("..")
        .expect("updated parent")
        .iter()
        .map(|entry| entry.expect("parent entry").file_name())
        .collect::<Vec<_>>();
    assert!(parent_names.iter().any(|name| name == "moved-child"));
}

#[test]
fn test_format_8mb_1fat() {
    let total_bytes = 8 * MB;
    let opts = fatfs::FormatVolumeOptions::new().fats(1);
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat16);
}

#[test]
fn test_format_50mb() {
    let total_bytes = 50 * MB;
    let opts = fatfs::FormatVolumeOptions::new();
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat16);
}

#[test]
fn test_format_2gb_512sec() {
    let total_bytes = 2 * 1024 * MB;
    let opts = fatfs::FormatVolumeOptions::new();
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
}

#[test]
fn test_format_1gb_4096sec() {
    let total_bytes = 1024 * MB;
    let opts = fatfs::FormatVolumeOptions::new().bytes_per_sector(4096);
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
}

#[test]
fn test_format_empty_volume_label() {
    let total_bytes = 2 * 1024 * MB;
    let opts = fatfs::FormatVolumeOptions::new();
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.volume_label(), "NO NAME");
    assert_eq!(fs.read_volume_label_from_root_dir().unwrap(), None);
}

#[test]
fn test_format_volume_label_and_id() {
    let total_bytes = 2 * 1024 * MB;
    let opts = fatfs::FormatVolumeOptions::new()
        .volume_id(1234)
        .volume_label(*b"VOLUMELABEL");
    let fs = test_format_fs(opts, total_bytes);
    assert_eq!(fs.volume_label(), "VOLUMELABEL");
    assert_eq!(
        fs.read_volume_label_from_root_dir().unwrap(),
        Some("VOLUMELABEL".to_string())
    );
    assert_eq!(fs.volume_id(), 1234);
}

#[test]
fn test_zero_root_dir_clusters() {
    init_logger();
    let total_bytes = 33 * MB;
    let opts = fatfs::FormatVolumeOptions::new().fat_type(FatType::Fat32);
    let fs = format_fs(opts, total_bytes);
    let root_dir = fs.root_dir();

    // create a bunch of files to force allocation of second root directory cluster (64 is combined size of LFN + SFN)
    let files_to_create = fs.cluster_size() as usize / 64 + 1;
    for i in 0..files_to_create {
        root_dir.create_file(&format!("f{}", i)).unwrap();
    }
    assert_eq!(root_dir.iter().count(), files_to_create);
}
