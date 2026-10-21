//! Multiple kits, each a self-contained directory under
//! `$XDG_DATA_HOME/odj/kits/<slug>/`: its own `kit.json` (pad mapping and
//! properties, see `kit.rs`) plus a `samples/` folder of `.wav` files it
//! owns. There's no separate registry file listing them — the directory
//! listing itself is the list (see [`list_kits`]); which one is active is
//! tracked separately, in `settings.rs`.

use std::path::PathBuf;

use odj_core::memory::data_dir;

use crate::kit::Kit;

pub fn kits_dir() -> PathBuf {
    data_dir().join("kits")
}

/// Every kit that currently exists, as `(directory, name)`, by name.
pub fn list_kits() -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(kits_dir()) else { return Vec::new() };
    let mut kits: Vec<(PathBuf, String)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|dir| dir.join("kit.json").is_file())
        .map(|dir| {
            let name = Kit::load(&dir.join("kit.json")).name;
            (dir, name)
        })
        .collect();
    kits.sort_by(|(_, a), (_, b)| a.cmp(b));
    kits
}

/// Lowercase, non-alphanumeric runs collapsed to a single `-`, trimmed;
/// `"kit"` if that leaves nothing (e.g. a name that was only punctuation).
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "kit".to_string() } else { slug }
}

/// [`slugify`], then `-2`, `-3`, … appended until it names a directory that
/// doesn't exist yet under [`kits_dir`].
fn unique_slug(name: &str) -> String {
    let base = slugify(name);
    let mut slug = base.clone();
    let mut n = 2;
    while kits_dir().join(&slug).exists() {
        slug = format!("{base}-{n}");
        n += 1;
    }
    slug
}

/// Creates a new, empty kit named `name` and returns its directory.
pub fn create_kit(name: &str) -> std::io::Result<PathBuf> {
    let dir = kits_dir().join(unique_slug(name));
    std::fs::create_dir_all(dir.join("samples"))?;
    Kit::new(name.to_string(), Default::default()).save(&dir.join("kit.json"))?;
    Ok(dir)
}

/// `XDG_DATA_HOME` is process-global, but `cargo test` runs tests on several
/// threads at once — anything (here, or in `app.rs`) that points it at a
/// scratch directory for the duration of a test holds this lock first, so
/// two such tests never race each other's idea of `data_dir()`.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_data_dir<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("odj-kits-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only, same pattern odj-player's tests already use;
        // serialized against other such tests by `ENV_LOCK` above.
        unsafe { std::env::set_var("XDG_DATA_HOME", &dir) };
        let result = f();
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        std::fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn slugify_handles_punctuation_and_spaces() {
        assert_eq!(slugify("My Kit!!"), "my-kit");
        assert_eq!(slugify("  Trap_808  "), "trap-808");
        assert_eq!(slugify("***"), "kit");
        assert_eq!(slugify(""), "kit");
    }

    #[test]
    fn create_kit_dedupes_names_on_disk() {
        with_temp_data_dir(|| {
            let a = create_kit("My Kit").unwrap();
            let b = create_kit("My Kit").unwrap();
            assert_ne!(a, b);
            assert!(a.ends_with("kits/my-kit"));
            assert!(b.ends_with("kits/my-kit-2"));
            assert!(a.join("samples").is_dir());
            assert!(a.join("kit.json").is_file());
        });
    }

    #[test]
    fn list_kits_reflects_the_directory() {
        with_temp_data_dir(|| {
            assert_eq!(list_kits(), Vec::new());
            create_kit("Zebra").unwrap();
            create_kit("Acoustic").unwrap();
            let names: Vec<_> = list_kits().into_iter().map(|(_, name)| name).collect();
            assert_eq!(names, vec!["Acoustic".to_string(), "Zebra".to_string()], "sorted by name");
        });
    }

}
