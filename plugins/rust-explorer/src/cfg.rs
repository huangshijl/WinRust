//! The file manager's own state: %APPDATA%\rust-explorer\settings.ini.
//! [window] place; [ui] the show switches; [view] one line per folder; [fold] the folded tile sections.
//! Older builds kept the same facts in window.txt / views.txt / nav-width.txt: the first read migrates them and leaves those
//! files alone. Every read sees the disk (several windows write concurrently); a save is read-merge-write of this one file.
use std::{env, fs, path::PathBuf};

fn dir() -> PathBuf { PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join("rust-explorer") }
pub fn file() -> PathBuf { dir().join("settings.ini") }

#[derive(Default)]
pub struct Ini { entries: Vec<(String, String, String)> } // section, key, value

impl Ini {
    fn parse(s: &str) -> Self {
        let mut out = Self::default();
        let mut sec = String::from("ui");
        for line in s.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                sec = name.trim().to_ascii_lowercase();
            } else if line.is_empty() || line.starts_with(';') {
                continue;
            } else if let Some((k, v)) = line.split_once('\t') {
                out.entries.push((sec.clone(), k.to_string(), v.to_string())); // paths keep any '=' harmless
            } else if let Some((k, v)) = line.split_once('=') {
                out.entries.push((sec.clone(), k.trim().to_ascii_lowercase(), v.to_string()));
            }
        }
        out
    }
    pub fn get(&self, sec: &str, key: &str) -> Option<&str> {
        let key = key.to_lowercase();
        self.entries.iter().find(|(s, k, _)| s == sec && *k == key).map(|(_, _, v)| v.as_str())
    }
    pub fn set(&mut self, sec: &str, key: &str, val: &str) {
        let key = key.to_lowercase();
        match self.entries.iter_mut().find(|(s, k, _)| s == sec && *k == key) {
            Some(e) => e.2 = val.to_string(),
            None => self.entries.push((sec.to_string(), key, val.to_string())),
        }
    }
    fn dump(&self) -> String {
        let mut s = String::new();
        for sec in ["window", "ui", "view", "fold"] {
            let rows: Vec<_> = self.entries.iter().filter(|e| e.0 == sec).collect();
            if rows.is_empty() { continue; }
            s.push_str(&format!("[{sec}]\n"));
            for (_, k, v) in rows {
                if sec == "view" { s.push_str(&format!("{k}\t{v}\n")); } else { s.push_str(&format!("{k}={v}\n")); }
            }
            s.push('\n');
        }
        s
    }
    pub fn save(&self) {
        let f = file();
        let _ = fs::create_dir_all(f.parent().unwrap_or(&f));
        let _ = fs::write(f, self.dump());
    }
}

/// settings.ini, or the old files imported into it on the very first run.
pub fn load() -> Ini {
    if let Ok(s) = fs::read_to_string(file()) { return Ini::parse(&s); }
    let mut ini = Ini::default();
    if let Ok(p) = fs::read_to_string(dir().join("window.txt")) {
        if !p.trim().is_empty() { ini.set("window", "place", p.trim()); }
    }
    if let Ok(w) = fs::read_to_string(dir().join("nav-width.txt")) {
        if let Ok(w) = w.trim().parse::<i32>() { ini.set("ui", "navw", &w.to_string()); }
    }
    if let Ok(v) = fs::read_to_string(dir().join("views.txt")) {
        for l in v.lines().filter(|l| !l.is_empty()) {
            let mut it = l.split('\t');
            let Some(key) = it.next() else { continue };
            let rest: Vec<&str> = it.collect();
            if key == "*" {
                for (k, x) in ["compact", "ext", "nav", "cap"].into_iter().zip(&rest) { ini.set("ui", k, x); }
            } else if rest.len() >= 7 {
                ini.set("view", key, &rest.join("\t"));
            }
        }
    }
    ini.save();
    ini
}

pub fn get(sec: &str, key: &str) -> Option<String> { load().get(sec, key).map(str::to_string) }
pub fn set(sec: &str, key: &str, val: &str) { let mut i = load(); i.set(sec, key, val); i.save(); }
pub fn boolean(sec: &str, key: &str) -> Option<bool> { get(sec, key).map(|v| v == "1") }
pub fn number(sec: &str, key: &str) -> Option<i32> { get(sec, key).and_then(|v| v.parse().ok()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dump_roundtrip_keeps_views_and_switches() {
        let text = "[ui]\nnav=1\ncap=0\n\n[view]\nc:\\a\t0\t3\t1\t0\t700\t200\t175\t120\n::home\t6\t0\t1\t3\t640\t200\t175\t96\n\n[fold]\n::home\t01\n";
        let ini = Ini::parse(text);
        assert_eq!(ini.get("ui", "nav"), Some("1"));
        assert_eq!(ini.get("ui", "cap"), Some("0"));
        assert_eq!(ini.get("view", "C:\\A"), Some("0\t3\t1\t0\t700\t200\t175\t120"));
        assert_eq!(ini.get("fold", "::home"), Some("01"));
        assert_eq!(Ini::parse(&ini.dump()).get("view", "::home"), Some("6\t0\t1\t3\t640\t200\t175\t96"));
    }

    #[test]
    fn a_path_with_an_equals_sign_survives() {
        let mut ini = Ini::default();
        ini.set("view", "c:\\a=b", "0\t0\t1\t0\t1\t2\t3\t4");
        let back = Ini::parse(&ini.dump());
        assert_eq!(back.get("view", "c:\\a=b"), Some("0\t0\t1\t0\t1\t2\t3\t4"));
    }
}
