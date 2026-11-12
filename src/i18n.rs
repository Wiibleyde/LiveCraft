//! Interface text. Every sentence a person can read lives in `locales/<code>.txt`, one
//! `key = text` per line, so translating LiveCraft means editing a text file, not code.
//! Code asks for text with `t!("key")`, or `t!("key", name = value)` to fill `{name}`.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::{Display, Write};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// Text for a key in the interface language; `t!("key", name = value)` fills `{name}`.
/// Keys are always literals, so a test can find every one of them in the code.
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::text($key)
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::text($key),
            &[$((stringify!($name), &$value as &dyn std::fmt::Display)),+],
        )
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Fr,
    En,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Fr, Lang::En];

    pub fn code(self) -> &'static str {
        match self {
            Lang::Fr => "fr",
            Lang::En => "en",
        }
    }

    /// Accepts a bare code or a regional one, as "fr-CA" or "en_GB.UTF-8".
    pub fn from_code(code: &str) -> Option<Lang> {
        let base = code.split(['-', '_', '.']).next().unwrap_or("").to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == base)
    }

    /// The language the computer speaks, or French, the language LiveCraft was written in.
    pub fn system() -> Lang {
        system_code().as_deref().and_then(Lang::from_code).unwrap_or(Lang::Fr)
    }

    /// Its own name in its own words, so that someone lost in a language they do not
    /// read still recognises theirs in the list.
    pub fn native_name(self) -> &'static str {
        lookup(self, "language.name").unwrap_or(self.code())
    }

    fn source(self) -> &'static str {
        match self {
            Lang::Fr => include_str!("../locales/fr.txt"),
            Lang::En => include_str!("../locales/en.txt"),
        }
    }
}

#[cfg(windows)]
fn system_code() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    // The low bits of a language identifier name the language, the high ones its region.
    let primary = unsafe { GetUserDefaultUILanguage() } & 0x3ff;
    match primary {
        0x0c => Some("fr".into()),
        0x09 => Some("en".into()),
        _ => None,
    }
}

#[cfg(not(windows))]
fn system_code() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|v| std::env::var(v).ok()).find(|v| !v.is_empty())
}

static CURRENT: AtomicU8 = AtomicU8::new(Lang::Fr as u8);

thread_local! {
    static HERE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// The window's thread keeps its own choice; threads behind it, as outputs reporting a
/// failure, use the shared one.
pub fn set_language(lang: Lang) {
    HERE.with(|h| h.set(Some(lang)));
    // Tests run many desks side by side in one process: one of them changing language
    // must not change the words the others are looking for.
    #[cfg(not(test))]
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn language() -> Lang {
    HERE.with(Cell::get).unwrap_or(if CURRENT.load(Ordering::Relaxed) == Lang::En as u8 {
        Lang::En
    } else {
        Lang::Fr
    })
}

type Catalog = HashMap<&'static str, Box<str>>;

fn catalog(lang: Lang) -> &'static Catalog {
    static FR: LazyLock<Catalog> = LazyLock::new(|| parse(Lang::Fr.source()).0);
    static EN: LazyLock<Catalog> = LazyLock::new(|| parse(Lang::En.source()).0);
    match lang {
        Lang::Fr => &FR,
        Lang::En => &EN,
    }
}

fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    catalog(lang).get(key).map(|v| &**v)
}

/// Every key with its text in one language, for tests that look at the whole window.
#[cfg(test)]
pub fn entries(lang: Lang) -> impl Iterator<Item = (&'static str, &'static str)> {
    catalog(lang).iter().map(|(k, v)| (*k, &**v))
}

/// A key missing from a translation falls back to French, then to the key itself, so a
/// gap shows as a readable word rather than nothing.
pub fn text(key: &'static str) -> &'static str {
    lookup(language(), key).or_else(|| lookup(Lang::Fr, key)).unwrap_or(key)
}

/// Replaces each `{name}` with its value. Braces that name nothing stay as they are.
pub fn fill(template: &str, values: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let named = after.find('}').and_then(|end| {
            let name = &after[..end];
            values.iter().find(|(n, _)| *n == name).map(|(_, v)| (v, end))
        });
        match named {
            Some((value, end)) => {
                let _ = write!(out, "{value}");
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `key = text` lines; `#` starts a comment line. In the text, `\n` is a line break and
/// `\\` a backslash. Returns the entries and what was wrong with the file.
fn parse(src: &'static str) -> (Catalog, Vec<String>) {
    let mut map = Catalog::new();
    let mut problems = Vec::new();
    for (n, line) in src.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(" =").map(|(k, v)| (k.trim(), v.trim())) else {
            problems.push(format!("line {}: no ` = `", n + 1));
            continue;
        };
        if key.is_empty() || key.contains(char::is_whitespace) {
            problems.push(format!("line {}: bad key `{key}`", n + 1));
            continue;
        }
        if map.insert(key, unescape(value).into()).is_some() {
            problems.push(format!("line {}: `{key}` twice", n + 1));
        }
    }
    (map, problems)
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(text: &str) -> BTreeSet<&str> {
        let mut found = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            rest = &rest[start + 1..];
            if let Some(end) = rest.find('}') {
                let name = &rest[..end];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    found.insert(name);
                }
            }
        }
        found
    }

    #[test]
    fn every_catalog_parses_cleanly() {
        for lang in Lang::ALL {
            let (_, problems) = parse(lang.source());
            assert!(problems.is_empty(), "{}.txt: {problems:#?}", lang.code());
        }
    }

    #[test]
    fn every_language_has_every_key_with_the_same_placeholders() {
        let reference = &parse(Lang::Fr.source()).0;
        for lang in Lang::ALL {
            let other = &parse(lang.source()).0;
            let missing: Vec<_> = reference.keys().filter(|k| !other.contains_key(*k)).collect();
            let extra: Vec<_> = other.keys().filter(|k| !reference.contains_key(*k)).collect();
            assert!(missing.is_empty(), "{}.txt lacks {missing:?}", lang.code());
            assert!(extra.is_empty(), "{}.txt has unknown {extra:?}", lang.code());
            for (key, text) in reference {
                let theirs = &other[key];
                assert_eq!(placeholders(text), placeholders(theirs), "{} in {}.txt", key, lang.code());
                assert!(!theirs.is_empty() || text.is_empty(), "{key} is empty in {}.txt", lang.code());
            }
        }
    }

    /// Every `t!` in the code names a key that exists, and gives exactly the values its
    /// text asks for: a typo would otherwise show the bare key in the window.
    #[test]
    fn the_code_asks_for_existing_keys_with_the_right_values() {
        let reference = &parse(Lang::Fr.source()).0;
        let mut used = BTreeSet::new();
        let mut checked = 0;
        for (file, code) in sources() {
            let mut rest = code.as_str();
            while let Some(at) = rest.find("t!(") {
                // `assert!(…)` and `concat!(…)` end the same way.
                let other_macro = rest[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
                // Formatting puts a long call's key on the next line.
                rest = rest[at + 3..].trim_start();
                if other_macro || !rest.starts_with('"') {
                    continue;
                }
                rest = &rest[1..];
                let Some(end) = rest.find('"') else { break };
                let key = &rest[..end];
                let args = macro_args(&rest[end + 1..]);
                let Some(text) = reference.get(key) else {
                    panic!("{file}: `{key}` is in no catalog");
                };
                let wanted = placeholders(text);
                let given: BTreeSet<&str> = args.iter().map(String::as_str).collect();
                assert_eq!(wanted, given, "{file}: values given to `{key}`");
                used.insert(key.to_string());
                checked += 1;
            }
        }
        assert!(checked > 100, "the search found too few keys ({checked})");
        let unused: Vec<_> =
            reference.keys().filter(|k| !used.contains(**k) && !k.starts_with("language.")).collect();
        assert!(unused.is_empty(), "keys no code uses: {unused:?}");
    }

    #[test]
    fn placeholders_are_filled_and_unknown_braces_kept() {
        let out = fill("{n} source(s) {x} {", &[("n", &3), ("y", &"no")]);
        assert_eq!(out, "3 source(s) {x} {");
        assert_eq!(unescape(r"a\nb\\c"), "a\nb\\c");
        assert_eq!(Lang::from_code("en_GB.UTF-8"), Some(Lang::En));
        assert_eq!(Lang::from_code("fr-CA"), Some(Lang::Fr));
        assert_eq!(Lang::from_code("de-DE"), None);
    }

    /// Source files, read from disk, without this module's own examples.
    fn sources() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("i18n.rs") {
                    out.push((path.display().to_string(), std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        out
    }

    /// Names given after the key in `t!("key", a = x, b = f(y, z))`: `["a", "b"]`.
    fn macro_args(after_key: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut depth = 0;
        let mut segment = String::new();
        let mut in_string = false;
        let mut chars = after_key.chars();
        let mut flush = |segment: &mut String| {
            if let Some((name, _)) = segment.split_once('=') {
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
            segment.clear();
        };
        while let Some(c) = chars.next() {
            if in_string {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' if depth == 0 => break,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => flush(&mut segment),
                _ if depth == 0 => segment.push(c),
                _ => {}
            }
        }
        flush(&mut segment);
        names
    }
}
