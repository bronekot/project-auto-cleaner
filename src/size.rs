use std::fs;
use std::io;
use std::path::Path;

#[derive(Default)]
struct SizeEstimate {
    bytes: u64,
    #[cfg(unix)]
    hard_links: std::collections::HashMap<(u64, u64), HardLinks>,
}

#[cfg(unix)]
struct HardLinks {
    found: u64,
    total: u64,
    bytes: u64,
}

impl SizeEstimate {
    fn add(&mut self, metadata: &fs::Metadata) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            // Allocated blocks account for sparse files, unlike logical length.
            let bytes = metadata.blocks().saturating_mul(512);
            if !metadata.is_dir() && metadata.nlink() > 1 {
                let links = self
                    .hard_links
                    .entry((metadata.dev(), metadata.ino()))
                    .or_insert(HardLinks {
                        found: 0,
                        total: metadata.nlink(),
                        bytes,
                    });
                links.found += 1;
            } else {
                self.bytes = self.bytes.saturating_add(bytes);
            }
        }
        #[cfg(not(unix))]
        if metadata.is_file() && !super::is_link_or_reparse(metadata) {
            self.bytes = self.bytes.saturating_add(metadata.len());
        }
    }

    fn total(self) -> u64 {
        #[cfg(unix)]
        {
            // Files still linked outside this directory do not release their blocks.
            self.hard_links
                .values()
                .filter(|links| links.found == links.total)
                .fold(self.bytes, |total, links| total.saturating_add(links.bytes))
        }
        #[cfg(not(unix))]
        {
            self.bytes
        }
    }
}

pub(super) fn estimate_directory_size(path: &Path) -> io::Result<u64> {
    fn visit(path: &Path, estimate: &mut SizeEstimate) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        estimate.add(&metadata);
        if metadata.is_dir() && !super::is_link_or_reparse(&metadata) {
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
