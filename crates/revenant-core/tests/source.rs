use revenant_core::source::{FileSource, Fragmented, Source, VolumeId, volume_of};
use revenant_core::{EngineError, Fragment};
use std::path::{Path, PathBuf};

fn make_file(dir: &Path) -> PathBuf {
    let path = dir.join("image.bin");
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, data).unwrap();
    path
}

#[test]
fn reads_middle_and_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    let s = FileSource::open(&path).unwrap();
    assert_eq!(s.len(), 1000);
    let mut b = [0u8; 10];
    assert_eq!(s.read_at(500, &mut b).unwrap(), 10);
    assert_eq!(b[0], (500 % 251) as u8);
    assert_eq!(s.read_at(995, &mut b).unwrap(), 5);
    assert_eq!(s.read_at(1000, &mut b).unwrap(), 0);
    assert_eq!(s.read_at(5000, &mut b).unwrap(), 0);
}

#[test]
fn never_modifies_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    let before = std::fs::read(&path).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    {
        let s = FileSource::open(&path).unwrap();
        let mut buf = vec![0u8; 256];
        let mut off = 0;
        while s.read_at(off, &mut buf).unwrap() > 0 {
            off += buf.len() as u64;
        }
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
}

#[test]
fn opens_read_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();
    let s = FileSource::open(&path).unwrap();
    assert_eq!(s.len(), 1000);
}

#[test]
fn same_volume_for_file_and_its_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_file(dir.path());
    assert_eq!(
        volume_of(dir.path()).unwrap(),
        FileSource::open(&path).unwrap().volume().unwrap()
    );
}

#[test]
fn opening_missing_file_gives_code() {
    let missing = tempfile::tempdir().unwrap().path().join("does-not-exist");
    assert_eq!(
        FileSource::open(&missing).err().unwrap().code(),
        "open_failed"
    );
}

/// What would open something for writing. `GENERIC_WRITE` is allowed once, in `source/windows.rs`, inside
/// `lock_volume_for_dismount` (the Windows volume lock exception, CLAUDE.md).
const BANNED: [&str; 9] = [
    "write(true)",
    ".append(",
    "create(true)",
    "create_new(",
    "GENERIC_WRITE",
    "GENERIC_ALL",
    "FILE_ALL_ACCESS",
    "FILE_WRITE_DATA",
    "FILE_APPEND_DATA",
];
const EXCEPTION_FN: &str = "fn lock_volume_for_dismount(";
const EXCEPTION_MARK: &str = "WRITE-ACCESS EXCEPTION (CLAUDE.md)";

/// Every banned string `text` holds, except the one write-access exception when `exception` allows it.
fn write_paths(text: &str, exception: bool) -> Vec<String> {
    let mut found: Vec<String> = BANNED
        .iter()
        .filter(|b| **b != "GENERIC_WRITE" && text.contains(*b))
        .map(|b| b.to_string())
        .collect();
    let lines: Vec<&str> = text.lines().collect();
    let writes: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].contains("GENERIC_WRITE"))
        .collect();
    let allowed = exception
        && writes.len() == 1
        && text.matches("GENERIC_WRITE").count() == 1
        && lines
            .iter()
            .position(|l| l.contains(EXCEPTION_FN))
            .is_some_and(|f| {
                // The function ends at its own closing brace: the first line that is just `}`.
                let end = lines[f + 1..]
                    .iter()
                    .position(|l| *l == "}")
                    .map_or(f, |n| f + 1 + n);
                f > 0 && lines[f - 1].contains(EXCEPTION_MARK) && (f..end).contains(&writes[0])
            });
    if !writes.is_empty() && !allowed {
        found.push(format!("GENERIC_WRITE on lines {writes:?}"));
    }
    found
}

#[test]
fn source_has_no_write_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in ["source", "device"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            files.push(entry.unwrap().path());
        }
    }
    // P14: every module that touches a source; one that does not exist yet is covered once it does.
    files.extend(
        ["identity.rs", "preflight.rs"]
            .iter()
            .map(|f| root.join(f))
            .filter(|p| p.exists()),
    );
    let windows = root.join("source/windows.rs");
    let mut checked = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let found = write_paths(&text, *path == windows);
        assert!(found.is_empty(), "{} contains {found:?}", path.display());
        if *path == windows {
            // The exception exists, in its one place (a refactor that drops the marker fails here).
            assert_eq!(
                text.matches("GENERIC_WRITE").count(),
                1,
                "{}",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(files.contains(&windows) && files.contains(&root.join("identity.rs")));
    assert!(checked >= 12, "{checked}");
}

#[test]
fn write_path_check_finds_misplaced_write_access() {
    let mark = format!("// {EXCEPTION_MARK}");
    let good = format!(
        "use x;\n{mark}\npub(crate) {EXCEPTION_FN}v: &str) {{\n    open(GENERIC_WRITE);\n}}\n\nfn other() {{}}\n"
    );
    assert!(write_paths(&good, true).is_empty());
    // Anywhere but the exception's file, or without the exception allowed, it is a write path.
    assert!(!write_paths(&good, false).is_empty());
    // Without the mark right above the function.
    let unmarked = good.replace(&mark, "// lock");
    assert!(!write_paths(&unmarked, true).is_empty());
    // Outside the function (after the next top-level item), or imported at the top.
    let outside = good.replace("    open(GENERIC_WRITE);", "    open(0);")
        + "pub fn more() { open(GENERIC_WRITE); }\n";
    assert!(!write_paths(&outside, true).is_empty());
    let imported = format!("use y::GENERIC_WRITE;\n{good}");
    assert!(!write_paths(&imported, true).is_empty());
    // Twice in the function, even on one line.
    let twice = good.replace("open(GENERIC_WRITE)", "open(GENERIC_WRITE | GENERIC_WRITE)");
    assert!(!write_paths(&twice, true).is_empty());
    // Right after the function's closing brace, before the next `fn`: outside it.
    let after = good
        .replace("    open(GENERIC_WRITE);", "    open(0);")
        .replace(
            "}\n\nfn other",
            "}\nconst W: u32 = GENERIC_WRITE;\n\nfn other",
        );
    assert!(!write_paths(&after, true).is_empty());
    // Other access rights that write are banned everywhere.
    for right in [
        "GENERIC_ALL",
        "FILE_ALL_ACCESS",
        "FILE_WRITE_DATA",
        "FILE_APPEND_DATA",
    ] {
        let other = good.replace(
            "open(GENERIC_WRITE)",
            &format!("open(GENERIC_WRITE | {right})"),
        );
        assert_eq!(write_paths(&other, true), [right]);
    }
    // The other banned strings are never excepted.
    let create = good.replace("open(GENERIC_WRITE)", "open(GENERIC_WRITE).create(true)");
    assert_eq!(write_paths(&create, true), ["create(true)"]);
}

/// The only files allowed `unsafe` (CLAUDE.md), relative to a crate's `src/`.
const UNSAFE_FILES: [&str; 2] = ["source/windows.rs", "source/macos_fd.rs"];

/// `text` with every comment and string or char literal blanked to spaces (newlines kept, so line numbers hold): what is
/// left is code. A lexer only as deep as the guard needs: line and nested block comments, plain, byte and raw strings,
/// char literals told from lifetimes.
fn code_only(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
    let mut i = 0;
    while i < c.len() {
        let next = c.get(i + 1).copied();
        if c[i] == '/' && next == Some('/') {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '/' && next == Some('*') {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
        } else if c[i] == 'r'
            && matches!(next, Some('"') | Some('#'))
            && (!ident_before(&c, i) || (c[i - 1] == 'b' && !ident_before(&c, i - 1)))
        {
            // A raw string r"…", r#"…"# or br"…" (the `b` stays as code, harmlessly).
            let mut j = i + 1;
            while c.get(j) == Some(&'#') {
                j += 1;
            }
            if c.get(j) != Some(&'"') {
                out.push(c[i]);
                i += 1;
                continue;
            }
            let hashes = j - i - 1;
            let close: Vec<char> = std::iter::once('"')
                .chain("#".repeat(hashes).chars())
                .collect();
            let mut k = j + 1;
            while k < c.len() && !c[k..].starts_with(&close) {
                k += 1;
            }
            let end = (k + close.len()).min(c.len());
            for &ch in &c[i..end] {
                blank(&mut out, ch);
            }
            i = end;
        } else if c[i] == '"' {
            blank(&mut out, c[i]);
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' && i + 1 < c.len() {
                    blank(&mut out, c[i]);
                    i += 1;
                }
                blank(&mut out, c[i]);
                i += 1;
            }
            if i < c.len() {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '\'' {
            // A char literal ('x', '\n', '\u{…}'), else a lifetime or label ('a) left as code.
            let end = if next == Some('\\') {
                (i + 2..c.len()).find(|&k| c[k] == '\'').map(|k| k + 1)
            } else if c.get(i + 2) == Some(&'\'') {
                Some(i + 3)
            } else {
                None
            };
            match end {
                Some(end) => {
                    for &ch in &c[i..end] {
                        blank(&mut out, ch);
                    }
                    i = end;
                }
                None => {
                    out.push(c[i]);
                    i += 1;
                }
            }
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// Whether the char before `i` continues an identifier (so an `r` there is not a raw-string prefix).
fn ident_before(c: &[char], i: usize) -> bool {
    i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_')
}

/// Every use of the `unsafe` keyword in `text` that breaks the rule: anywhere when the file is not `allowed`; in an
/// allowed file, one without a `// SAFETY:` comment on one of the 3 lines above it. Comments and literals do not count,
/// nor identifiers that merely contain the word (`is_unsafe_char`).
fn unsafe_uses(text: &str, allowed: bool) -> Vec<String> {
    let code = code_only(text);
    let original: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for (n, line) in code.lines().enumerate() {
        let c: Vec<char> = line.chars().collect();
        let word: Vec<char> = "unsafe".chars().collect();
        let is_ident = |ch: char| ch.is_alphanumeric() || ch == '_';
        let hit = (0..c.len()).any(|i| {
            c[i..].starts_with(&word)
                && (i == 0 || !is_ident(c[i - 1]))
                && c.get(i + word.len()).is_none_or(|&ch| !is_ident(ch))
        });
        if !hit {
            continue;
        }
        let justified = original[n.saturating_sub(3)..n]
            .iter()
            .any(|l| l.trim_start().starts_with("// SAFETY:"));
        if !allowed {
            found.push(format!("line {}: unsafe outside the allowed files", n + 1));
        } else if !justified {
            found.push(format!("line {}: unsafe without // SAFETY: above", n + 1));
        }
    }
    found
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The rule broken in each crate source under `crates_dir` (`<crate>/src/**/*.rs`): `(file, problems)`.
fn unsafe_report(crates_dir: &Path) -> Vec<(PathBuf, Vec<String>)> {
    let mut files = Vec::new();
    for krate in std::fs::read_dir(crates_dir).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    files.sort();
    let mut report = Vec::new();
    for path in files {
        let rel = path
            .components()
            .skip_while(|c| c.as_os_str() != "src")
            .skip(1)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let in_core = path.starts_with(crates_dir.join("revenant-core"));
        let allowed = in_core && UNSAFE_FILES.contains(&rel.as_str());
        let text = std::fs::read_to_string(&path).unwrap();
        let problems = unsafe_uses(&text, allowed);
        if !problems.is_empty() {
            report.push((path, problems));
        }
    }
    report
}

#[test]
fn unsafe_only_in_allowed_files() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let report = unsafe_report(crates_dir);
    assert!(report.is_empty(), "{report:#?}");
    // The allowed files exist and do use `unsafe`, each block justified (a rename would silently drop the check).
    for file in UNSAFE_FILES {
        let path = crates_dir.join("revenant-core/src").join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!unsafe_uses(&text, false).is_empty(), "{file}");
    }
}

#[test]
fn unsafe_check_finds_misplaced_unsafe() {
    // A crate tree on disk: one allowed file (justified and not), one file that must have none.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("demo/src/source");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(dir.path().join("revenant-core/src/source")).unwrap();
    std::fs::write(
        dir.path().join("revenant-core/src/source/macos_fd.rs"),
        "fn a() {\n    // SAFETY: fine.\n    unsafe { x() };\n}\n",
    )
    .unwrap();
    std::fs::write(src.join("macos_fd.rs"), "fn b() { unsafe { x() } }\n").unwrap();
    let report = unsafe_report(dir.path());
    // The same file name in another crate is not allowed.
    assert_eq!(report.len(), 1, "{report:#?}");
    assert!(report[0].0.ends_with("demo/src/source/macos_fd.rs"));

    let ok = "fn a() {\n    // SAFETY: the pointer is valid.\n    let v = unsafe { *p };\n}\n";
    assert!(unsafe_uses(ok, true).is_empty());
    assert_eq!(unsafe_uses(ok, false).len(), 1);
    // The SAFETY comment up to 3 lines above, not 4.
    let three = "// SAFETY: a\n// b\n// c\nunsafe { x() };\n";
    assert!(unsafe_uses(three, true).is_empty());
    let four = "// SAFETY: a\n// b\n// c\n// d\nunsafe { x() };\n";
    assert_eq!(unsafe_uses(four, true).len(), 1);
    // Not justified by a SAFETY that is not a comment line of its own.
    let inline = "let s = \"// SAFETY: no\";\nunsafe { x() };\n";
    assert_eq!(unsafe_uses(inline, true).len(), 1);
    // Every form of the keyword counts.
    for form in [
        "unsafe fn f() {}",
        "unsafe impl Send for X {}",
        "x = unsafe{y};",
    ] {
        assert_eq!(unsafe_uses(form, false).len(), 1, "{form}");
    }
    // Comments, strings, chars and identifiers do not (ruling P3).
    for not_code in [
        "// unsafe { x() }\n",
        "/// unsafe here\n",
        "//! unsafe here\n",
        "/* unsafe /* nested */ unsafe */ fn a() {}\n",
        "let s = \"unsafe { x }\";\n",
        "let s = \"a \\\" unsafe\";\n",
        "let s = r#\"unsafe \" unsafe\"#;\n",
        "let s = b\"unsafe\";\n",
        "let s = br\"\\\"; let t = \"unsafe\";\n",
        "fn is_unsafe_char(c: char) -> bool { c == '\"' }\nfn f<'a>(x: &'a str) {}\n",
        "let unsafe_count = 1; let x = not_unsafe;\n",
    ] {
        assert!(unsafe_uses(not_code, false).is_empty(), "{not_code}");
    }
    // A char literal holding a quote does not hide code after it.
    assert_eq!(
        unsafe_uses("let q = '\"'; unsafe { x() };\n", false).len(),
        1
    );
}

/// Fails every read that touches `[bad, bad + 1)`, reporting the first bad byte as the error offset.
struct FailsAt {
    inner: FileSource,
    bad: u64,
}

impl Source for FailsAt {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset <= self.bad && self.bad < offset + buf.len() as u64 {
            return Err(EngineError::Read {
                offset: self.bad,
                cause: std::io::Error::other("injected"),
            });
        }
        self.inner.read_at(offset, buf)
    }

    fn volume(&self) -> Option<VolumeId> {
        Some(VolumeId(7))
    }
}

#[test]
fn fragmented_view_maps_reads_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.bin");
    let src: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &src).unwrap();
    let file = FileSource::open(&path).unwrap();
    let pieces = vec![
        Fragment {
            start: 100,
            len: 10,
        },
        Fragment { start: 50, len: 20 },
    ];
    let view = Fragmented::new(&file, pieces.clone());
    assert_eq!(view.len(), 30);
    assert_eq!(view.volume(), file.volume());

    let mut b = [0u8; 10];
    assert_eq!(view.read_at(5, &mut b).unwrap(), 10);
    assert_eq!(b[..5], src[105..110]);
    assert_eq!(b[5..], src[50..55]);
    let mut all = [0u8; 30];
    assert_eq!(view.read_at(0, &mut all).unwrap(), 30);
    assert_eq!(all[..10], src[100..110]);
    assert_eq!(all[10..], src[50..70]);
    // Past the logical end: short, then nothing.
    assert_eq!(view.read_at(25, &mut b).unwrap(), 5);
    assert_eq!(b[..5], src[65..70]);
    assert_eq!(view.read_at(30, &mut b).unwrap(), 0);
    assert_eq!(view.read_at(u64::MAX, &mut b).unwrap(), 0);

    assert_eq!(
        view.absolute(5, 10),
        vec![
            Fragment { start: 105, len: 5 },
            Fragment { start: 50, len: 5 },
        ]
    );
    assert_eq!(view.absolute(0, 30), pieces);
    assert_eq!(view.absolute(12, 3), vec![Fragment { start: 52, len: 3 }]);
    assert_eq!(
        view.absolute(20, 100),
        vec![Fragment { start: 60, len: 10 }]
    );
    assert_eq!(view.absolute(30, 5), vec![]);
    // Pieces that touch in the source merge back into one.
    let touching = Fragmented::new(
        &file,
        vec![
            Fragment { start: 0, len: 512 },
            Fragment {
                start: 512,
                len: 512,
            },
        ],
    );
    assert_eq!(
        touching.absolute(100, 800),
        vec![Fragment {
            start: 100,
            len: 800
        }]
    );

    let failing = FailsAt {
        inner: FileSource::open(&path).unwrap(),
        bad: 60,
    };
    let view = Fragmented::new(&failing, pieces);
    assert_eq!(view.volume(), Some(VolumeId(7)));
    match view.read_at(0, &mut all) {
        Err(EngineError::Read { offset, .. }) => assert_eq!(offset, 60),
        other => panic!("expected a read error, got {other:?}"),
    }
    // A read that misses the bad byte succeeds.
    assert_eq!(view.read_at(0, &mut b).unwrap(), 10);
}

#[test]
fn fragmented_view_clamps_pieces_to_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.bin");
    let src: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &src).unwrap();
    let file = FileSource::open(&path).unwrap();
    // The first piece runs 104 bytes past the end; the last starts past it.
    let view = Fragmented::new(
        &file,
        vec![
            Fragment {
                start: 4000,
                len: 200,
            },
            Fragment { start: 50, len: 20 },
            Fragment {
                start: 5000,
                len: 10,
            },
        ],
    );
    assert_eq!(view.len(), 96 + 20);
    let mut all = [0u8; 200];
    assert_eq!(view.read_at(0, &mut all).unwrap(), 116);
    assert_eq!(all[..96], src[4000..]);
    assert_eq!(all[96..116], src[50..70]);
    assert_eq!(
        view.absolute(90, 100),
        vec![
            Fragment {
                start: 4090,
                len: 6
            },
            Fragment { start: 50, len: 20 },
        ]
    );
}

#[cfg(unix)]
#[test]
fn only_a_regular_file_opens() {
    // Checked on the opened handle, so a node swapped in after any caller's check is still refused; a FIFO is opened
    // without waiting for a writer.
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let fifo_path = fifo.clone();
    std::thread::spawn(move || {
        let r = FileSource::open(&fifo_path)
            .map(|_| ())
            .map_err(|e| e.code());
        let _ = tx.send(r);
    });
    let r = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("opening a FIFO never waits for a writer");
    assert_eq!(r, Err("open_failed"));
    for node in [Path::new("/dev/null"), dir.path()] {
        assert_eq!(
            FileSource::open(node).err().map(|e| e.code()),
            Some("open_failed"),
            "{node:?}"
        );
    }
}
