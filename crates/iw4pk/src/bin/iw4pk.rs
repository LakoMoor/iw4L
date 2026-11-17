//! `iw4pk pack`: turn an installed MW2 into browser shards.
//!
//! ```text
//! iw4pk pack --game <MW2 dir> --out <dir> [--lang russian] [--level 19]
//!            --map mp_rust=trace_rust.txt --map mp_boneyard=trace_bone.txt ...
//! iw4pk ls <shard.iw4pk>
//! ```
//!
//! A trace file is what `IW4L_PACK_TRACE=<file>` makes the engine write while a
//! map is played: `archive<TAB>entry`, one IWD entry per line. Entries every
//! map touched go to `common.iw4pk`; entries one map touched go to that map's
//! shard, so a player downloads `common` plus the map they pick.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use iw4pk::write::Writer;
use iw4pk::{Pack, ReadAt};

/// Zones every MP session loads regardless of map.
const COMMON_ZONES: &[&str] = &[
    "code_pre_gfx_mp",
    "localized_code_pre_gfx_mp",
    "patch_code_pre_gfx_mp",
    "code_post_gfx_mp",
    "localized_code_post_gfx_mp",
    "common_mp",
    "localized_common_mp",
    "ui_mp",
    "localized_ui_mp",
    "patch_mp",
];

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("iw4pk: {msg}");
    std::process::exit(2);
}

struct FileSource(File, u64);

impl ReadAt for FileSource {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, iw4pk::Error> {
        use std::os::windows::fs::FileExt;
        let mut buf = vec![0; len];
        let mut done = 0;
        while done < len {
            let n = self
                .0
                .seek_read(&mut buf[done..], offset + done as u64)
                .map_err(|e| iw4pk::Error::Io(e.to_string()))?;
            if n == 0 {
                return Err(iw4pk::Error::Truncated);
            }
            done += n;
        }
        Ok(buf)
    }
    fn len(&self) -> Result<u64, iw4pk::Error> {
        Ok(self.1)
    }
}

fn already_compressed(entry: &str) -> bool {
    let e = entry.to_ascii_lowercase();
    e.ends_with(".mp3") || e.ends_with(".ogg") || e.ends_with(".opus") || e.ends_with(".flac")
}

fn load_trace(path: &Path) -> BTreeSet<(String, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| die(format!("{}: {e}", path.display())));
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, e)| (a.to_owned(), e.to_owned()))
        .collect()
}

fn add_zone(w: &mut Writer<BufWriter<File>>, game: &Path, lang: &str, name: &str) -> bool {
    let rel = format!("zone/{lang}/{name}.ff");
    let path = game.join(&rel);
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    // Fastfiles are deflate streams already: zstd cannot help, store as-is.
    w.add(&rel, &bytes, true).unwrap_or_else(|e| die(e));
    true
}

fn pack(args: &[String]) {
    let mut game = None;
    let mut out = None;
    let mut lang = "russian".to_owned();
    let mut level = 19;
    let mut maps: Vec<(String, PathBuf)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| die(format!("{a} needs a value")));
        match a.as_str() {
            "--game" => game = Some(PathBuf::from(val())),
            "--out" => out = Some(PathBuf::from(val())),
            "--lang" => lang = val(),
            "--level" => level = val().parse().unwrap_or_else(|_| die("bad --level")),
            "--map" => {
                let v = val();
                let (m, t) = v.split_once('=').unwrap_or_else(|| die("--map NAME=TRACE"));
                maps.push((m.to_owned(), PathBuf::from(t)));
            }
            other => die(format!("unknown argument {other}")),
        }
    }
    let game = game.unwrap_or_else(|| die("--game required"));
    let out = out.unwrap_or_else(|| die("--out required"));
    if maps.is_empty() {
        die("at least one --map NAME=TRACE");
    }
    fs::create_dir_all(&out).unwrap_or_else(|e| die(e));

    let traces: Vec<(String, BTreeSet<(String, String)>)> = maps
        .iter()
        .map(|(m, t)| (m.clone(), load_trace(t)))
        .collect();
    let mut users: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for (m, set) in &traces {
        for key in set {
            users.entry(key.clone()).or_default().push(m);
        }
    }

    // Shard name for each IWD entry: sole user's map, or common.
    let shard_of = |key: &(String, String)| -> String {
        match users[key].as_slice() {
            [only] if traces.len() > 1 => (*only).to_owned(),
            _ => "common".to_owned(),
        }
    };

    let mut shard_names: Vec<String> = vec!["common".into()];
    shard_names.extend(maps.iter().map(|(m, _)| m.clone()));
    let mut writers: BTreeMap<String, Writer<BufWriter<File>>> = BTreeMap::new();
    for s in &shard_names {
        let f = File::create(out.join(format!("{s}.iw4pk"))).unwrap_or_else(|e| die(e));
        writers.insert(
            s.clone(),
            Writer::new(BufWriter::new(f), level).unwrap_or_else(|e| die(e)),
        );
    }

    for z in COMMON_ZONES {
        let w = writers.get_mut("common").unwrap();
        if add_zone(w, &game, &lang, z) {
            println!("common  zone/{lang}/{z}.ff");
        }
    }
    for (m, _) in &maps {
        let w = writers.get_mut(m).unwrap();
        for z in [m.clone(), format!("{m}_load")] {
            if add_zone(w, &game, &lang, &z) {
                println!("{m:<12} zone/{lang}/{z}.ff");
            }
        }
    }

    // IWD entries, archive by archive so each zip is opened once.
    let mut by_archive: BTreeMap<&str, Vec<&(String, String)>> = BTreeMap::new();
    for key in users.keys() {
        by_archive.entry(key.0.as_str()).or_default().push(key);
    }
    let mut missing = 0usize;
    for (archive, keys) in by_archive {
        let path = game.join("main").join(archive);
        let file = File::open(&path).unwrap_or_else(|e| die(format!("{}: {e}", path.display())));
        let mut zip = zip::ZipArchive::new(file).unwrap_or_else(|e| die(format!("{archive}: {e}")));
        for key in keys {
            let mut data = Vec::new();
            match zip.by_name(&key.1) {
                Ok(mut e) => {
                    e.read_to_end(&mut data).unwrap_or_else(|e| die(e));
                }
                Err(_) => {
                    missing += 1;
                    continue;
                }
            }
            let shard = shard_of(key);
            let vpath = format!("main/{archive}/{}", key.1);
            writers
                .get_mut(&shard)
                .unwrap()
                .add(&vpath, &data, already_compressed(&key.1))
                .unwrap_or_else(|e| die(e));
        }
    }
    if missing > 0 {
        eprintln!("warning: {missing} traced entries were not found in their archive");
    }

    let mut total = 0;
    for (name, w) in writers {
        let (inner, index) = w.finish().unwrap_or_else(|e| die(e));
        drop(inner);
        let size = fs::metadata(out.join(format!("{name}.iw4pk"))).map_or(0, |m| m.len());
        total += size;
        println!(
            "{name}.iw4pk: {} entries, {:.1} MB",
            index.entries.len(),
            size as f64 / 1e6
        );
    }
    println!("total {:.1} MB", total as f64 / 1e6);
}

fn ls(args: &[String]) {
    let path = args.first().unwrap_or_else(|| die("ls <shard>"));
    let f = File::open(path).unwrap_or_else(|e| die(e));
    let len = f.metadata().map_or(0, |m| m.len());
    let pack = Pack::open(FileSource(f, len)).unwrap_or_else(|e| die(e));
    for e in &pack.index.entries {
        let stored: u64 = e.chunks.iter().map(|c| u64::from(c.stored_len)).sum();
        println!("{:>12} {:>12}  {}", e.raw_len, stored, e.path);
    }
}

/// Join shards into one file the page can slice: shard bytes back to back, then a
/// JSON table of contents, its length (u64 LE) and the magic `IW4BNDL1`.
fn bundle(args: &[String]) {
    let mut out = None;
    let mut parts: Vec<(String, PathBuf)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            out = it.next().map(PathBuf::from);
        } else {
            let (name, path) = a.split_once('=').unwrap_or_else(|| die("NAME=SHARD"));
            parts.push((name.to_owned(), PathBuf::from(path)));
        }
    }
    let out = out.unwrap_or_else(|| die("--out required"));
    if parts.is_empty() {
        die("bundle --out FILE name=shard ...");
    }
    let mut w = std::io::BufWriter::new(File::create(&out).unwrap_or_else(|e| die(e)));
    let mut offset = 0u64;
    let mut toc = Vec::new();
    for (name, path) in &parts {
        let mut r = File::open(path).unwrap_or_else(|e| die(e));
        let len = std::io::copy(&mut r, &mut w).unwrap_or_else(|e| die(e));
        toc.push(format!(
            "{{\"name\":\"{name}\",\"offset\":{offset},\"len\":{len}}}"
        ));
        offset += len;
    }
    let json = format!("{{\"v\":1,\"parts\":[{}]}}", toc.join(","));
    use std::io::Write;
    w.write_all(json.as_bytes()).unwrap_or_else(|e| die(e));
    w.write_all(&(json.len() as u64).to_le_bytes()).unwrap_or_else(|e| die(e));
    w.write_all(b"IW4BNDL1").unwrap_or_else(|e| die(e));
    w.flush().unwrap_or_else(|e| die(e));
    println!("{} parts, {:.1} MB -> {}", parts.len(), (offset + json.len() as u64) as f64 / 1e6, out.display());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pack") => pack(&args[1..]),
        Some("ls") => ls(&args[1..]),
        Some("bundle") => bundle(&args[1..]),
        _ => die("usage: iw4pk pack|ls ..."),
    }
}
