//! Multiple kits, each a self-contained directory under [`kits_dir`]: its
//! own `kit.json` (pad mapping and properties, see `kit.rs`) plus a
//! `samples/` folder of `.wav` files it owns. There's no separate registry
//! file listing them — the directory listing itself is the list (see
//! [`list_kits`]); which one is active is tracked separately, in
//! `settings.rs`.

use std::path::{Path, PathBuf};

use odj_core::memory::data_dir;

use crate::kit::Kit;

/// `$XDG_DATA_HOME/odj/kits/` — always the global kits folder, even with
/// crates active: active crates are a pure, merged, read-only browsing
/// set (see `Settings.active_crates`), never a write target. A new kit
/// always lands here; picking an existing one from a crate's own
/// `kits/` (see `odj_core::crates::Crate::kits_dir`) is handled
/// explicitly by the kit picker (`kit_browser.rs`), not by this function.
pub fn kits_dir() -> PathBuf {
    data_dir().join("kits")
}

/// Every kit that currently exists in the global folder, as `(directory,
/// name)`, by name.
pub fn list_kits() -> Vec<(PathBuf, String)> {
    list_kits_in(&kits_dir())
}

/// Like [`list_kits`], but for an arbitrary folder — an active crate's
/// own `kits/` (see `odj_core::crates::Crate::kits_dir`), for the
/// merged, tagged picker in `kit_browser.rs`.
pub fn list_kits_in(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
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
        // XDG_CONFIG_HOME is isolated too, for tests that touch settings
        // directly — `kits_dir` itself no longer reads them at all.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &dir);
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let result = f();
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_CONFIG_HOME");
        }
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

    #[test]
    fn kits_dir_is_always_the_global_one_regardless_of_active_crates() {
        with_temp_data_dir(|| {
            assert_eq!(kits_dir(), data_dir().join("kits"));

            let crate_dir = std::env::temp_dir().join(format!("odj-kits-test-crate-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&crate_dir);
            odj_core::crates::Crate::create(crate_dir.clone(), "Set").unwrap();
            let mut settings = odj_core::memory::load_settings();
            settings.active_crates = vec!["whatever".to_string()];
            odj_core::memory::save_settings(&settings).unwrap();

            assert_eq!(kits_dir(), data_dir().join("kits"), "a new kit always lands in the global folder");
            std::fs::remove_dir_all(&crate_dir).unwrap();
        });
    }

}
