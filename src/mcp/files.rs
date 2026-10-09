//! Local copies of generated memes for the stdio server, where the returned
//! URL points at a `memegen serve` that usually isn't running.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Saved memes older than this are deleted.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The platform cache directory, e.g. `~/Library/Caches/memegen` on macOS
/// or `~/.cache/memegen` on Linux.
pub fn default_dir() -> PathBuf {
    let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|home| PathBuf::from(home).join("Library/Caches"))
    } else if cfg!(windows) {
        var("LOCALAPPDATA").map(PathBuf::from)
    } else {
        var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("memegen")
}

/// `{template}-{hash}.{extension}`; the same request maps to the same file.
pub fn file_name(template_id: &str, lines: &[String], font: &str, extension: &str) -> String {
    let mut hasher = DefaultHasher::new();
    (template_id, lines, font, extension).hash(&mut hasher);
    format!("{template_id}-{:016x}.{extension}", hasher.finish())
}

/// Whether `name` looks like a file from [`file_name`], so a sweep never
/// touches anything else in a shared directory.
fn is_saved_meme(name: &str) -> bool {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let Some((template_id, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    !template_id.is_empty()
        && hash.len() == 16
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && crate::settings::ALLOWED_EXTENSIONS.contains(&extension)
}

/// Delete saved memes in `dir` last written more than `max_age` ago; returns
/// how many were removed.
pub fn sweep(dir: &Path, max_age: Duration) -> io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(is_saved_meme) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let expired = metadata.is_file()
            && metadata
                .modified()
                .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > max_age);
        if expired && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn file_names_are_stable_and_distinct() {
        let a = file_name("fry", &lines(&["a", "b"]), "", "gif");
        assert_eq!(a, file_name("fry", &lines(&["a", "b"]), "", "gif"));
        assert_ne!(a, file_name("fry", &lines(&["a", "c"]), "", "gif"));
        assert_ne!(a, file_name("fry", &lines(&["a", "b"]), "comic", "gif"));
        assert!(a.starts_with("fry-") && a.ends_with(".gif"));
        assert!(is_saved_meme(&a));
        assert!(is_saved_meme(&file_name("ds-oprah", &[], "", "webp")));
    }

    #[test]
    fn only_saved_memes_match() {
        assert!(!is_saved_meme("notes.png"));
        assert!(!is_saved_meme("fry-0123456789abcdef.txt"));
        assert!(!is_saved_meme("fry-0123456789abcdeg.png"));
        assert!(!is_saved_meme("-0123456789abcdef.png"));
    }

    #[test]
    fn sweep_removes_only_old_saved_memes() {
        let dir = std::env::temp_dir().join(format!("memegen-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join(file_name("fry", &[], "", "png"));
        let fresh = dir.join(file_name("fry", &[], "", "gif"));
        let other = dir.join("keep.png");
        for path in [&old, &fresh, &other] {
            std::fs::write(path, b"image").unwrap();
        }
        let week_ago = SystemTime::now() - MAX_AGE - Duration::from_secs(60);
        for path in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(week_ago)
                .unwrap();
        }

        assert_eq!(sweep(&dir, MAX_AGE).unwrap(), 1);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(sweep(&dir, MAX_AGE).unwrap(), 0);
    }
}
