use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

/// Volume (device) and file number.
type FileId = (u64, u64);

#[derive(Default)]
struct SizeEstimate {
    bytes: u64,
    hard_links: HashMap<FileId, HardLinks>,
}

struct HardLinks {
    found: u64,
    total: u64,
    bytes: u64,
}

/// Allocated size of one entry and, for a file with several hard links, its
/// identity and link count.
struct EntrySize {
    bytes: u64,
    hard_link: Option<(FileId, u64)>,
}

impl SizeEstimate {
    fn add(&mut self, path: &Path, metadata: &fs::Metadata) {
        let entry = entry_size(path, metadata);
        match entry.hard_link {
            Some((id, total)) => {
                self.hard_links
                    .entry(id)
                    .or_insert(HardLinks {
                        found: 0,
                        total,
                        bytes: entry.bytes,
                    })
                    .found += 1;
            }
            None => self.bytes = self.bytes.saturating_add(entry.bytes),
        }
    }

    fn total(self) -> u64 {
        // Files still linked outside this directory do not release their space.
        self.hard_links
            .values()
            .filter(|links| links.found == links.total)
            .fold(self.bytes, |total, links| total.saturating_add(links.bytes))
    }
}

pub(super) fn estimate_directory_size(path: &Path) -> io::Result<u64> {
    fn visit(path: &Path, estimate: &mut SizeEstimate) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        estimate.add(path, &metadata);
        // `is_dir` is false for symlinks and junctions, so links are not followed.
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(&entry?.path(), estimate)?;
            }
        }
        Ok(())
    }

    let mut estimate = SizeEstimate::default();
    visit(path, &mut estimate)?;
    Ok(estimate.total())
}

#[cfg(unix)]
fn entry_size(_path: &Path, metadata: &fs::Metadata) -> EntrySize {
    use std::os::unix::fs::MetadataExt;

    // Allocated blocks account for sparse files, unlike logical length.
    let bytes = metadata.blocks().saturating_mul(512);
    let hard_link = (!metadata.is_dir() && metadata.nlink() > 1)
        .then(|| ((metadata.dev(), metadata.ino()), metadata.nlink()));
    EntrySize { bytes, hard_link }
}

#[cfg(windows)]
fn entry_size(path: &Path, metadata: &fs::Metadata) -> EntrySize {
    // Directories and links take little space and need extra calls to measure.
    if !metadata.is_file() {
        return EntrySize {
            bytes: 0,
            hard_link: None,
        };
    }
    windows_file_size(path).unwrap_or(EntrySize {
        bytes: metadata.len(),
        hard_link: None,
    })
}

#[cfg(windows)]
fn windows_file_size(path: &Path) -> Option<EntrySize> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_STANDARD_INFO,
    };

    // Metadata-only access: file contents are never read.
    let file = fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let handle = file.as_raw_handle();
    let mut identity = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let mut standard = MaybeUninit::<FILE_STANDARD_INFO>::uninit();
    // SAFETY: `handle` stays open for both calls, and each buffer has the size
    // and layout its call expects; they are read only after the calls succeed.
    let (identity, standard) = unsafe {
        if GetFileInformationByHandle(handle, identity.as_mut_ptr()) == 0
            || GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                standard.as_mut_ptr().cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            ) == 0
        {
            return None;
        }
        (identity.assume_init(), standard.assume_init())
    };
    let links = u64::from(identity.nNumberOfLinks);
    let id = (
        u64::from(identity.dwVolumeSerialNumber),
        (u64::from(identity.nFileIndexHigh) << 32) | u64::from(identity.nFileIndexLow),
    );
    Some(EntrySize {
        bytes: u64::try_from(standard.AllocationSize).unwrap_or(0),
        hard_link: (links > 1).then_some((id, links)),
    })
}

#[cfg(not(any(unix, windows)))]
fn entry_size(_path: &Path, metadata: &fs::Metadata) -> EntrySize {
    EntrySize {
        bytes: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        hard_link: None,
    }
}
