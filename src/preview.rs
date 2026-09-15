//! Cached preview sources for the picker and navsplat panes.
//!
//! Both applets show a window of a source file beside the selection and used
//! to read and split the whole file on every frame. The cache keeps the split
//! lines of recently shown files and re-reads a file only when its size or
//! modification time changes, so redraws, preview scrolling, and moving back
//! to a candidate in the same file cost a `stat` instead of a read.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Most files kept at once; the least recently loaded entry is dropped first.
pub const CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

#[derive(Debug)]
struct CachedFile {
    path: PathBuf,
    stamp: FileStamp,
    lines: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PreviewCache {
    entries: Vec<CachedFile>,
    reads: u64,
}

impl PreviewCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The file's lines, split as `str::lines` does, or `None` when it cannot
    /// be read. Unreadable files are not cached, so a later lookup retries.
    pub fn lines(&mut self, path: &Path) -> Option<&[String]> {
        let stamp = FileStamp::of(path)?;
        let index = match self.entries.iter().position(|entry| entry.path == path) {
            Some(index) if self.entries[index].stamp == stamp => index,
            Some(index) => {
                self.entries.remove(index);
                self.load(path, stamp)?
            }
            None => self.load(path, stamp)?,
        };
        Some(self.entries[index].lines.as_slice())
    }

    /// Files read from disk so far; lookups served from the cache do not count.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn load(&mut self, path: &Path, stamp: FileStamp) -> Option<usize> {
        let contents = fs::read_to_string(path).ok()?;
        self.reads += 1;
        if self.entries.len() >= CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(CachedFile {
            path: path.to_path_buf(),
            stamp,
            lines: contents.lines().map(str::to_owned).collect(),
        });
        Some(self.entries.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::Duration;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(test_name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "innards-preview-{test_name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn strings(lines: &[String]) -> Vec<String> {
        lines.to_vec()
    }

    #[test]
    fn repeated_lookups_are_served_without_rereading() {
        let scratch = ScratchDir::new("hit");
        let path = scratch.file("a.txt", "alpha\nbeta\n");
        let mut cache = PreviewCache::new();
        let expected = vec!["alpha".to_string(), "beta".to_string()];

        assert_eq!(strings(cache.lines(&path).unwrap()), expected);
        assert_eq!(strings(cache.lines(&path).unwrap()), expected);

        assert_eq!(cache.reads(), 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_rewritten_file_is_reread_even_at_the_same_length() {
        let scratch = ScratchDir::new("stale");
        let path = scratch.file("a.txt", "alpha\n");
        let mut cache = PreviewCache::new();
        assert_eq!(
            strings(cache.lines(&path).unwrap()),
            vec!["alpha".to_string()]
        );

        // Same length, so only the modification time distinguishes the rewrite.
        fs::write(&path, "gamma\n").unwrap();
        let later = SystemTime::now() + Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            strings(cache.lines(&path).unwrap()),
            vec!["gamma".to_string()]
        );
        assert_eq!(cache.reads(), 2);
        assert_eq!(cache.len(), 1, "a stale entry is replaced, not duplicated");

        fs::write(&path, "gamma\ndelta\n").unwrap();
        assert_eq!(
            cache.lines(&path).unwrap().len(),
            2,
            "a length change invalidates as well"
        );
        assert_eq!(cache.reads(), 3);
    }

    #[test]
    fn unreadable_files_are_not_cached() {
        let scratch = ScratchDir::new("missing");
        let path = scratch.0.join("missing.txt");
        let mut cache = PreviewCache::new();

        assert!(cache.lines(&path).is_none());
        assert!(cache.is_empty());
        assert_eq!(cache.reads(), 0);

        fs::write(&path, "now here\n").unwrap();
        assert_eq!(
            strings(cache.lines(&path).unwrap()),
            vec!["now here".to_string()]
        );
        assert_eq!(cache.reads(), 1);
    }

    #[test]
    fn empty_files_stay_distinguishable_from_unreadable_ones() {
        let scratch = ScratchDir::new("empty");
        let path = scratch.file("empty.txt", "");
        let mut cache = PreviewCache::new();

        assert_eq!(cache.lines(&path).unwrap().len(), 0);
        assert_eq!(cache.reads(), 1);
    }

    #[test]
    fn the_oldest_entry_is_evicted_at_capacity() {
        let scratch = ScratchDir::new("capacity");
        let mut cache = PreviewCache::new();
        let paths: Vec<PathBuf> = (0..=CAPACITY)
            .map(|index| scratch.file(&format!("{index}.txt"), &format!("{index}\n")))
            .collect();
        for path in &paths {
            assert!(cache.lines(path).is_some());
        }
        let loaded = CAPACITY as u64 + 1;

        assert_eq!(cache.len(), CAPACITY);
        assert_eq!(cache.reads(), loaded);
        assert!(cache.lines(&paths[CAPACITY]).is_some());
        assert_eq!(cache.reads(), loaded, "the newest file is still cached");
        assert!(cache.lines(&paths[0]).is_some());
        assert_eq!(cache.reads(), loaded + 1, "the oldest file was evicted");
    }
}
