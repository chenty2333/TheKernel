use core::time::Duration;

use axfs_ng_vfs::{DeviceId, Metadata, MetadataUpdate, NodeType, VfsError, VfsResult};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};

use super::{ff, fs::FatFilesystemInner};

pub fn dos_to_unix(date: fatfs::DateTime) -> Duration {
    let time = date.time;
    let Some(date) = NaiveDate::from_ymd_opt(
        date.date.year as _,
        date.date.month as _,
        date.date.day as _,
    ) else {
        return Duration::default();
    };
    let Some(date) = date.and_hms_milli_opt(
        time.hour as _,
        time.min as _,
        time.sec as _,
        time.millis as _,
    ) else {
        return Duration::default();
    };
    let Some(datetime) = Utc.from_local_datetime(&date).single() else {
        return Duration::default();
    };
    datetime
        .signed_duration_since(DateTime::UNIX_EPOCH)
        .to_std()
        .unwrap_or_default()
}

pub fn unix_to_dos(datetime: Duration) -> fatfs::DateTime {
    let Some(dt) = chrono::Duration::from_std(datetime)
        .ok()
        .and_then(|duration| DateTime::UNIX_EPOCH.checked_add_signed(duration))
    else {
        return fatfs::DateTime::new(
            fatfs::Date::new(2107, 12, 31),
            fatfs::Time::new(23, 59, 59, 999),
        );
    };
    let dt = dt.naive_local();

    if dt.year() < 1980 {
        return fatfs::DateTime::new(fatfs::Date::new(1980, 1, 1), fatfs::Time::new(0, 0, 0, 0));
    }
    if dt.year() > 2107 {
        return fatfs::DateTime::new(
            fatfs::Date::new(2107, 12, 31),
            fatfs::Time::new(23, 59, 59, 999),
        );
    }

    fatfs::DateTime::new(
        fatfs::Date::new(dt.year() as _, dt.month() as _, dt.day() as _),
        fatfs::Time::new(
            dt.hour() as _,
            dt.minute() as _,
            dt.second() as _,
            dt.and_utc().timestamp_subsec_millis() as _,
        ),
    )
}

pub fn file_metadata(
    fs: &FatFilesystemInner,
    file: &ff::File,
    inode: u64,
    node_type: NodeType,
) -> VfsResult<Metadata> {
    let size = match node_type {
        NodeType::RegularFile => file.size().map(u64::from).ok_or(VfsError::Io)?,
        _ => file.size().map_or(0, u64::from),
    };
    let block_size = fs.inner.bytes_per_sector();
    let mode = if node_type == NodeType::Directory {
        fs.mount_options.dir_mode
    } else {
        fs.mount_options.file_mode
    };
    Ok(Metadata {
        inode,
        device: 0,
        nlink: 1,
        mode,
        node_type,
        uid: fs.mount_options.uid,
        gid: fs.mount_options.gid,
        project_id: 0,
        size,
        block_size: block_size as _,
        blocks: size.div_ceil(512),
        rdev: DeviceId::default(),
        atime: dos_to_unix(fatfs::DateTime::new(
            file.accessed(),
            fatfs::Time::new(0, 0, 0, 0),
        )).into(),
        btime: dos_to_unix(file.created()).into(),
        mtime: dos_to_unix(file.modified()).into(),
        ctime: dos_to_unix(file.created()).into(),
    })
}

pub fn update_file_metadata(file: &mut ff::File, update: MetadataUpdate) -> VfsResult<()> {
    if update.mode.is_some()
        || update.owner.is_some()
        || update.rdev.is_some()
        || update.ctime.is_some()
    {
        return Err(VfsError::Unsupported);
    }
    if let Some(atime) = update.atime {
        #[allow(deprecated)]
        file.set_accessed(
            unix_to_dos(atime.try_into_duration().ok_or(VfsError::InvalidInput)?).date,
        );
    }
    if let Some(mtime) = update.mtime {
        #[allow(deprecated)]
        file.set_modified(unix_to_dos(
            mtime.try_into_duration().ok_or(VfsError::InvalidInput)?,
        ));
    }
    Ok(())
}

pub fn into_vfs_err<E>(err: fatfs::Error<E>) -> VfsError {
    use fatfs::Error::*;
    match err {
        AlreadyExists => VfsError::AlreadyExists,
        CorruptedFileSystem => VfsError::InvalidData,
        DirectoryIsNotEmpty => VfsError::DirectoryNotEmpty,
        InvalidFileNameLength => VfsError::NameTooLong,
        InvalidInput => VfsError::InvalidInput,
        UnsupportedFileNameCharacter => VfsError::InvalidData,
        NotEnoughMemory => VfsError::NoMemory,
        NotEnoughSpace => VfsError::StorageFull,
        NotFound => VfsError::NotFound,
        UnexpectedEof | WriteZero => VfsError::Io,
        _ => VfsError::Io,
    }
}
