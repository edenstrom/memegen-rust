//! Local copies of generated memes for the stdio server, where the returned
//! URL points at a `memegen serve` that usually isn't running.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::render::AnimateText;

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

/// `{template}-{hash}.{extension}`; the same request maps to the same key.
pub fn key(
    template_id: &str,
    lines: &[String],
    font: &str,
    extension: &str,
    animate_text: AnimateText,
) -> String {
    let mut hasher = DefaultHasher::new();
    (template_id, lines, font, extension).hash(&mut hasher);
    // Only hashed when set, so keys of memes saved before it existed hold.
    match animate_text {
        AnimateText::Off => {}
        AnimateText::Characters => true.hash(&mut hasher),
        AnimateText::Words => "words".hash(&mut hasher),
    }
    format!("{template_id}-{:016x}.{extension}", hasher.finish())
}

/// `{date}_{time}_{key}` in local time, e.g.
/// `2026-10-09_143012_fry-0123456789abcdef.png`, so names sort by creation.
pub fn file_name(key: &str, now: SystemTime) -> String {
    format!("{}_{key}", timestamp(now))
}

/// A meme already saved in `dir` under `key`, from any date. Its modified
/// time is refreshed so the sweep keeps it as long as it's being reused.
pub fn reuse(dir: &Path, key: &str) -> Option<PathBuf> {
    let path = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| strip_timestamp(name) == key)
        })?
        .path();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .ok()?;
    Some(path)
}

/// `YYYY-MM-DD_HHMMSS` in local time.
fn timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}_{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Strips a leading `YYYY-MM-DD_HHMMSS_` from `name`, if it has one.
fn strip_timestamp(name: &str) -> &str {
    let bytes = name.as_bytes();
    let is_timestamp = bytes.len() > 18
        && bytes[..18].iter().enumerate().all(|(i, &byte)| match i {
            4 | 7 => byte == b'-',
            10 | 17 => byte == b'_',
            _ => byte.is_ascii_digit(),
        });
    if is_timestamp { &name[18..] } else { name }
}

/// Whether `name` looks like a file from [`file_name`] (or the undated names
/// used before), so a sweep never touches anything else in a shared directory.
fn is_saved_meme(name: &str) -> bool {
    let Some((stem, extension)) = strip_timestamp(name).rsplit_once('.') else {
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

    fn name(template_id: &str, text: &[&str], font: &str, extension: &str) -> String {
        file_name(
            &key(template_id, &lines(text), font, extension, AnimateText::Off),
            SystemTime::now(),
        )
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memegen-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        let a = key("fry", &lines(&["a", "b"]), "", "gif", AnimateText::Off);
        assert_eq!(
            a,
            key("fry", &lines(&["a", "b"]), "", "gif", AnimateText::Off)
        );
        assert_ne!(
            a,
            key("fry", &lines(&["a", "c"]), "", "gif", AnimateText::Off)
        );
        assert_ne!(
            a,
            key("fry", &lines(&["a", "b"]), "comic", "gif", AnimateText::Off)
        );
        let characters = key(
            "fry",
            &lines(&["a", "b"]),
            "",
            "gif",
            AnimateText::Characters,
        );
        let words = key("fry", &lines(&["a", "b"]), "", "gif", AnimateText::Words);
        assert_ne!(a, characters);
        assert_ne!(a, words);
        assert_ne!(characters, words);
        assert!(a.starts_with("fry-") && a.ends_with(".gif"));
        assert!(is_saved_meme(&a));
        assert!(is_saved_meme(&name("fry", &["a"], "", "gif")));
        assert!(is_saved_meme(&name("ds-oprah", &[], "", "webp")));
    }

    #[test]
    fn file_names_sort_by_time() {
        let earlier = SystemTime::now();
        let later = earlier + Duration::from_secs(24 * 60 * 60 + 1);
        let a = file_name(&key("zzz", &[], "", "png", AnimateText::Off), earlier);
        let b = file_name(&key("aaa", &[], "", "png", AnimateText::Off), later);
        assert!(a < b, "{a} should sort before {b}");
        let date = &a[..10];
        assert!(date.starts_with("20") && date.as_bytes()[4] == b'-');
    }

    #[test]
    fn only_saved_memes_match() {
        assert!(is_saved_meme("fry-0123456789abcdef.png"));
        assert!(is_saved_meme("2026-10-09_143012_fry-0123456789abcdef.png"));
        assert!(!is_saved_meme("notes.png"));
        assert!(!is_saved_meme("2026-10-09_143012_notes.png"));
        assert!(!is_saved_meme("fry-0123456789abcdef.txt"));
        assert!(!is_saved_meme("fry-0123456789abcdeg.png"));
        assert!(!is_saved_meme("-0123456789abcdef.png"));
        assert!(!is_saved_meme("2026-10-09_143012_-0123456789abcdef.png"));
    }

    #[test]
    fn reuses_saved_memes_from_any_date() {
        let dir = temp_dir("reuse");
        let fry = key("fry", &lines(&["a"]), "", "png", AnimateText::Off);
        assert_eq!(reuse(&dir, &fry), None);

        let week_ago = SystemTime::now() - MAX_AGE + Duration::from_secs(60);
        let saved = dir.join(file_name(&fry, week_ago));
        std::fs::write(&saved, b"image").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&saved)
            .unwrap()
            .set_modified(week_ago)
            .unwrap();
        assert_eq!(reuse(&dir, &fry), Some(saved.clone()));
        let modified = std::fs::metadata(&saved).unwrap().modified().unwrap();
        assert!(modified > week_ago + Duration::from_secs(60));

        let legacy = key("fry", &lines(&["b"]), "", "png", AnimateText::Off);
        std::fs::write(dir.join(&legacy), b"image").unwrap();
        assert_eq!(reuse(&dir, &legacy), Some(dir.join(&legacy)));
        assert_eq!(
            reuse(
                &dir,
                &key("fry", &lines(&["a"]), "", "gif", AnimateText::Off)
            ),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sweep_removes_only_old_saved_memes() {
        let dir = temp_dir("sweep");
        let old = dir.join(name("fry", &[], "", "png"));
        let fresh = dir.join(name("fry", &[], "", "gif"));
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
