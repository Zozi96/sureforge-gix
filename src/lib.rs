//! In-process git primitives for the SureForge work-tree fingerprint.
//!
//! Each function answers one git question the Python orchestration in
//! `sureforge_hook.fingerprint()` asks, and returns a small structure, never
//! git's text. Any failure raises, and the caller then reruns the whole
//! fingerprint on the git CLI.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gix::bstr::{BString, ByteSlice};
use gix::dir::entry::{Kind, Status as DirStatus};
use gix::dir::walk::EmissionMode;
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn err(e: Box<dyn std::error::Error + Send + Sync>) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

fn text(path: &[u8]) -> Res<String> {
    Ok(std::str::from_utf8(path)?.to_owned())
}

fn open(root: &str) -> Res<gix::Repository> {
    Ok(gix::open(root)?)
}

/// Windows canonical paths are verbatim (`\\?\C:\x`, `\\?\UNC\host\share`); git and Python print `C:\x`.
fn plain(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Like Python's `Path.resolve()` for a path whose last component may not exist yet.
fn real(path: &Path) -> Res<String> {
    let resolved: PathBuf = match std::fs::canonicalize(path) {
        Ok(p) => plain(p),
        Err(_) => {
            let parent = path.parent().ok_or("path has no parent")?;
            plain(std::fs::canonicalize(parent)?).join(path.file_name().ok_or("path has no name")?)
        }
    };
    Ok(resolved.to_str().ok_or("path is not UTF-8")?.to_owned())
}

/// The batched `rev-parse`: (toplevel, index, common dir, info/exclude, objects).
fn paths_impl(cwd: &str) -> Res<(String, String, String, String, String)> {
    let repo = gix::discover(cwd)?;
    let top = repo.workdir().ok_or("bare repository")?;
    let common = repo.common_dir();
    Ok((
        real(top)?,
        real(&repo.index_path())?,
        real(common)?,
        real(&common.join("info").join("exclude"))?,
        real(&common.join("objects"))?,
    ))
}

/// `worktree list --porcelain` minus the main worktree: each linked worktree's path.
fn linked_worktrees_impl(root: &str) -> Res<Vec<String>> {
    let repo = open(root)?;
    let mut out = Vec::new();
    for proxy in repo.worktrees()? {
        out.push(proxy.base()?.to_str().ok_or("path is not UTF-8")?.to_owned());
    }
    out.sort();
    Ok(out)
}

/// One submodule as `git status --porcelain=v2` reports it: (path, commit changed, tracked changes, untracked files).
type SubmoduleFacts = (String, bool, bool, bool);

/// `status --untracked-files=all --ignore-submodules=none`: every untracked file and submodule state,
/// whatever `status.showUntrackedFiles` says. Its `no` drops the dirwalk entirely, so it is rebuilt here.
/// `ignored` also emits ignored paths, fully ignored directories collapsed, as `ls-files -o -i --directory`.
fn status_platform(repo: &gix::Repository, ignored: bool) -> Res<gix::status::Platform<'_, gix::progress::Discard>> {
    let mut dirwalk = repo.dirwalk_options()?.emit_untracked(EmissionMode::Matching);
    if ignored {
        dirwalk = dirwalk.emit_ignored(Some(EmissionMode::CollapseDirectory)).emit_empty_directories(true);
    }
    Ok(repo
        .status(gix::progress::Discard)?
        .index_worktree_submodules(gix::status::Submodule::Given {
            ignore: gix::submodule::config::Ignore::None,
            check_dirty: false,
        })
        .index_worktree_options_mut(|opts| opts.dirwalk_options = Some(dirwalk)))
}

/// `status --porcelain=v2 -z --untracked-files=all --ignore-submodules=none`, as facts:
/// whether anything is listed, the untracked paths (directories end in `/`), and submodule states.
fn status_impl(root: &str, threads: Option<usize>) -> Res<(bool, Vec<String>, Vec<SubmoduleFacts>)> {
    Ok(status_walk(root, threads, false)?.0)
}

type StatusFacts = (bool, Vec<String>, Vec<SubmoduleFacts>);

/// The status facts, and with `ignored` the ignored listing from the same directory walk.
fn status_walk(root: &str, threads: Option<usize>, ignored: bool) -> Res<(StatusFacts, Vec<String>)> {
    let repo = open(root)?;
    let mut ignored_paths = Vec::new();
    let iter = status_platform(&repo, ignored)?
        .index_worktree_options_mut(|opts| opts.thread_limit = threads)
        .into_iter(None::<BString>)?;
    let mut changed = false;
    let mut untracked = Vec::new();
    let mut submodules = Vec::new();
    for item in iter {
        match item? {
            gix::status::Item::TreeIndex(_) => changed = true,
            gix::status::Item::IndexWorktree(WorktreeItem::Modification { rela_path, status, .. }) => match status {
                EntryStatus::NeedsUpdate(_) => {}
                EntryStatus::Change(Change::SubmoduleModification(sub)) => {
                    let commit = sub.index_id != sub.checked_out_head_id;
                    let (mut modified, mut new) = (false, false);
                    for change in sub.changes.iter().flatten() {
                        match change {
                            gix::status::Item::IndexWorktree(WorktreeItem::DirectoryContents { entry, .. })
                                if entry.status == DirStatus::Untracked =>
                            {
                                new = true
                            }
                            gix::status::Item::IndexWorktree(WorktreeItem::Modification {
                                status: EntryStatus::NeedsUpdate(_),
                                ..
                            }) => {}
                            _ => modified = true,
                        }
                    }
                    if commit || modified || new {
                        changed = true;
                        submodules.push((text(&rela_path)?, commit, modified, new));
                    }
                }
                _ => changed = true,
            },
            gix::status::Item::IndexWorktree(WorktreeItem::DirectoryContents { entry, .. }) => {
                let empty = entry.property == Some(gix::dir::entry::Property::EmptyDirectory);
                let mut path = text(&entry.rela_path)?;
                if matches!(entry.disk_kind, Some(Kind::Directory | Kind::Repository)) {
                    path.push('/');
                }
                match entry.status {
                    // git status -uall never lists an empty directory; the walk emits it only for ls-files.
                    DirStatus::Untracked if !empty => {
                        changed = true;
                        untracked.push(path);
                    }
                    DirStatus::Ignored(_) => ignored_paths.push(path),
                    _ => {}
                }
            }
            gix::status::Item::IndexWorktree(_) => changed = true,
        }
    }
    untracked.sort();
    submodules.sort();
    ignored_paths.sort();
    Ok(((changed, untracked, submodules), ignored_paths))
}

/// Index entries `fingerprint()` reads: every stage of a path with a skip-worktree or
/// assume-unchanged stage, and every gitlink. The full listing is `tracked()`.
fn tracked_special_impl(root: &str) -> Res<Vec<Tracked>> {
    let repo = open(root)?;
    let index = repo.index_or_empty()?;
    let flag = gix::index::entry::Flags::SKIP_WORKTREE | gix::index::entry::Flags::ASSUME_VALID;
    let flagged: std::collections::HashSet<&gix::bstr::BStr> = index
        .entries()
        .iter()
        .filter(|e| e.flags.intersects(flag))
        .map(|e| e.path(&index))
        .collect();
    let mut out = Vec::new();
    for entry in index.entries() {
        let path = entry.path(&index);
        if entry.mode.bits() == 0o160000 || flagged.contains(path) {
            out.push((
                text(path)?,
                entry.mode.bits(),
                entry.id.to_hex().to_string(),
                entry.stage_raw(),
                entry.flags.contains(gix::index::entry::Flags::SKIP_WORKTREE),
                entry.flags.contains(gix::index::entry::Flags::ASSUME_VALID),
            ));
        }
    }
    Ok(out)
}

/// One index entry as `ls-files -s -v` shows it: (path, mode, oid, stage, skip-worktree, assume-unchanged).
type Tracked = (String, u32, String, u32, bool, bool);

fn tracked_impl(root: &str) -> Res<Vec<Tracked>> {
    let repo = open(root)?;
    let index = repo.index_or_empty()?;
    let mut out = Vec::with_capacity(index.entries().len());
    for entry in index.entries() {
        out.push((
            text(entry.path(&index))?,
            entry.mode.bits(),
            entry.id.to_hex().to_string(),
            entry.stage_raw(),
            entry.flags.contains(gix::index::entry::Flags::SKIP_WORKTREE),
            entry.flags.contains(gix::index::entry::Flags::ASSUME_VALID),
        ));
    }
    Ok(out)
}

/// `ls-files -o -i --exclude-standard --directory -z`: ignored untracked paths, fully ignored directories as `dir/`.
/// The same walk as `status_and_ignored`, which lists untracked files one by one so ignored files inside an
/// untracked directory still show.
fn ignored_untracked_impl(root: &str) -> Res<Vec<String>> {
    Ok(status_walk(root, None, true)?.1)
}

/// True when the filesystem holding `git_dir` finds it under a case-swapped name. Nothing is written.
fn case_insensitive_fs(git_dir: &Path) -> bool {
    let Some(name) = git_dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let swapped: String = name
        .chars()
        .map(|c| if c.is_lowercase() { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
        .collect();
    swapped != name && git_dir.with_file_name(swapped).exists()
}

/// True when `dir` holds a `.git` directory with HEAD, or a `.git` file starting with `gitdir:`.
fn holds_repo(dir: &Path) -> bool {
    let marker = dir.join(".git");
    match std::fs::symlink_metadata(&marker) {
        Ok(meta) if meta.is_dir() => marker.join("HEAD").exists(),
        Ok(meta) if meta.is_file() => std::fs::read(&marker).is_ok_and(|bytes| bytes.starts_with(b"gitdir:")),
        _ => false,
    }
}

/// Untracked paths under the directory `rel` as `ls-files -o` lists them: regular files and
/// symlinks, a nested repository as `path/` and not entered.
fn files_under(workdir: &Path, rel: &str, tracked: &HashSet<String>, out: &mut Vec<String>) -> Res<()> {
    let mut dirs = vec![rel.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(workdir.join(&dir))? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| "path is not UTF-8")?;
            if name == ".git" {
                continue;
            }
            let path = format!("{dir}/{name}");
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if holds_repo(&workdir.join(&path)) {
                    out.push(format!("{path}/"));
                } else {
                    dirs.push(path);
                }
            } else if (kind.is_file() || kind.is_symlink()) && !tracked.contains(&path) {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// The work-tree-relative path of an ignore source, as `_exclude_source_is_tracked` compares it.
fn source_rel(workdir_real: &Path, source: &Path) -> Option<String> {
    let resolved = real(source).ok()?;
    let rel = Path::new(&resolved).strip_prefix(workdir_real).ok()?;
    Some(rel.to_str()?.replace('\\', "/"))
}

/// The exclude rule `check-ignore -v` names for `path`: its source (None for a rule without one)
/// and whether it excludes; None when no rule matches. git names the outermost excluded
/// ancestor's rule; gix's stack prefers the nearest, so the ancestors are asked first, from the top.
fn ignore_rule(stack: &mut gix::AttributeStack<'_>, workdir_real: &Path, path: &str) -> Res<Option<(Option<String>, bool)>> {
    let trimmed = path.trim_end_matches('/');
    let source = |found: &gix::ignore::search::Match<'_>| found.source.map(|source| source_rel(workdir_real, source).unwrap_or_default());
    for (at, _) in trimmed.match_indices('/') {
        let platform = stack.at_path(Path::new(&trimmed[..at]), Some(gix::index::entry::Mode::DIR))?;
        if let Some(found) = platform.matching_exclude_pattern().filter(|found| !found.pattern.is_negative()) {
            return Ok(Some((source(&found), true)));
        }
    }
    let mode = if path.ends_with('/') { gix::index::entry::Mode::DIR } else { gix::index::entry::Mode::FILE };
    let platform = stack.at_path(Path::new(trimmed), Some(mode))?;
    Ok(platform.matching_exclude_pattern().map(|found| (source(&found), !found.pattern.is_negative())))
}

/// `check-ignore -v` on a directory entry, which carries its slash: the rule the stack finds for
/// `dir`, else one that names `dir/` with the slash (`dir/*`), which the stack cannot see. The
/// slash lists come in git's order: deepest `.gitignore` first, then info/exclude, then global.
fn dir_entry_rule(
    stack: &mut gix::AttributeStack<'_>,
    workdir: &Path,
    workdir_real: &Path,
    outside: &[(PathBuf, Vec<u8>)],
    path: &str,
    case: gix::glob::pattern::Case,
) -> Res<Option<(Option<String>, bool)>> {
    let trimmed = path.trim_end_matches('/');
    if let Some(rule) = ignore_rule(stack, workdir_real, path)? {
        return Ok(Some(rule));
    }
    let parse = gix::ignore::search::Ignore { support_precious: false };
    let mut search = gix::ignore::Search::default();
    for (source, bytes) in outside.iter().rev() {
        search.add_patterns_buffer(bytes, source.clone(), None, parse)?;
    }
    let mut dirs = vec![""];
    dirs.extend(trimmed.match_indices('/').map(|(at, _)| &trimmed[..at]));
    for dir in dirs {
        let file = workdir.join(dir).join(".gitignore");
        if file.is_file() {
            search.add_patterns_buffer(&std::fs::read(&file)?, file, Some(workdir), parse)?;
        }
    }
    let with_slash = format!("{trimmed}/");
    Ok(search
        .pattern_matching_relative_path(with_slash.as_str().into(), Some(true), case)
        .map(|found| (found.source.map(|source| source_rel(workdir_real, source).unwrap_or_default()), !found.pattern.is_negative())))
}

fn excluded(stack: &mut gix::AttributeStack<'_>, workdir_real: &Path, path: &str) -> Res<bool> {
    Ok(ignore_rule(stack, workdir_real, path)?.is_some_and(|(_, excludes)| excludes))
}

/// The ignored paths under the directory `dir`, which is not itself excluded, as
/// `ls-files -o -i --directory` lists them: an excluded directory as `path/`, an excluded file.
fn ignored_under(
    stack: &mut gix::AttributeStack<'_>,
    workdir: &Path,
    workdir_real: &Path,
    dir: &str,
    tracked: &HashSet<String>,
    out: &mut Vec<String>,
) -> Res<()> {
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(workdir.join(&dir))? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| "path is not UTF-8")?;
            if name == ".git" {
                continue;
            }
            let path = format!("{dir}/{name}");
            let kind = entry.file_type()?;
            if kind.is_dir() {
                let collapsed = format!("{path}/");
                if excluded(stack, workdir_real, &collapsed)? {
                    out.push(collapsed);
                } else if !holds_repo(&workdir.join(&path)) {
                    dirs.push(path);
                }
            } else if (kind.is_file() || kind.is_symlink()) && !tracked.contains(&path) && excluded(stack, workdir_real, &path)? {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// The pathspecs from the `!` lines of each tracked gitignore behind a collapsed directory,
/// as `_negation_pathspecs` builds them.
fn negation_pathspecs(workdir: &Path, tracked_dirs: &[(String, String)]) -> Res<Vec<String>> {
    let prefixes: Vec<String> = tracked_dirs.iter().map(|(dir, _)| dir.clone()).collect();
    let mut specs: Vec<String> = Vec::new();
    let mut read = HashSet::new();
    for (_, source) in tracked_dirs {
        if !read.insert(source.clone()) {
            continue;
        }
        let file = workdir.join(source);
        let content = if file.is_file() { String::from_utf8_lossy(&std::fs::read(&file)?).into_owned() } else { String::new() };
        let base = Path::new(source).parent().and_then(Path::to_str).unwrap_or("").replace('\\', "/");
        for line in content.lines() {
            let line = line.trim();
            if line.len() < 2 || !line.starts_with('!') {
                continue;
            }
            let pattern = line[1..].trim_start_matches('/');
            if pattern.is_empty() || pattern.split('/').any(|part| part == "..") {
                continue;
            }
            let anchored = line[1..].contains('/');
            let rel = if base.is_empty() { pattern.to_owned() } else { format!("{base}/{pattern}") };
            let spec = if anchored {
                if !prefixes.iter().any(|prefix| rel.starts_with(prefix.as_str())) {
                    continue;
                }
                rel
            } else {
                format!(":(glob)**/{pattern}")
            };
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
    }
    Ok(specs)
}

/// Python's `fnmatch.fnmatchcase(name, pattern)` for a pattern whose only wildcard is `*`
/// (any run, `/` included).
fn star_match(name: &str, pattern: &str) -> bool {
    let (s, p) = (name.as_bytes(), pattern.as_bytes());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while i < s.len() {
        if j < p.len() && p[j] != b'*' && p[j] == s[i] {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == b'*' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(at) = star {
            j = at + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    p[j..].iter().all(|c| *c == b'*')
}

/// `_exclude_might_match`: false only where it is easy to prove that no rule in `text` can
/// exclude a path the negation `specs` name. It answers as Python does, except that a CRLF
/// line, which Python's universal newlines read without its `\r`, makes it answer true.
/// (Python's GIT_ICASE_PATHSPECS check is absent: the preflight sends that environment to the CLI.)
fn exclude_might_match(text: &str, specs: &[String], fold: bool, workdir: &Path) -> bool {
    // A directory names every file below it; a missing path names nothing for git to list.
    let plain_file = |spec: &String| {
        !spec.ends_with('/') && std::fs::symlink_metadata(workdir.join(spec)).map_or(true, |meta| !meta.is_dir())
    };
    let literal = |spec: &String| {
        !spec.starts_with(':')
            && spec.bytes().all(|b| (0x20..0x7f).contains(&b))
            && !spec.contains(['*', '?', '[', ']', '\\'])
            && !spec.split('/').any(|part| part == "." || part == "..") // git normalises `nm/./a` to `nm/a`
            && plain_file(spec)
    };
    if !specs.iter().all(literal) {
        return true;
    }
    let simple = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '*' | '/');
    let mut rules = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.chars().all(simple) || line.contains("**") || line.ends_with('/') {
            return true;
        }
        rules.push(if fold { line.to_ascii_lowercase() } else { line.to_owned() });
    }
    specs.iter().any(|spec| {
        let spec = if fold { spec.to_ascii_lowercase() } else { spec.clone() };
        let parts: Vec<&str> = spec.split('/').filter(|part| !part.is_empty()).collect();
        rules.iter().any(|rule| {
            let anchored = rule.trim_start_matches('/');
            if anchored.contains('/') {
                (1..=parts.len()).any(|end| star_match(&parts[..end].join("/"), anchored))
            } else {
                parts.iter().any(|part| star_match(part, anchored))
            }
        })
    })
}

/// The outside exclude files `_outside_exclude_files` reads, with their text, when they hold a rule.
/// A file that exists but cannot be read is an error; a missing one holds nothing.
fn outside_exclude_files(repo: &gix::Repository, workdir: &Path, info_exclude: &Path) -> Res<Vec<(PathBuf, Vec<u8>)>> {
    let mut files = vec![info_exclude.to_owned()];
    // `git config --path` prints nothing for an empty value, which the CLI path cannot use: it decides.
    if repo.config_snapshot().string("core.excludesFile").is_some_and(|value| value.is_empty()) {
        return Err("core.excludesFile is empty".into());
    }
    match repo.config_snapshot().trusted_path("core.excludesFile")? {
        Some(path) => files.push(if path.is_absolute() { path } else { workdir.join(path) }),
        None => match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            Some(xdg) => files.push(PathBuf::from(xdg).join("git").join("ignore")),
            None => files.push(PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".config/git/ignore")),
        },
    }
    let mut usable = Vec::new();
    for path in files {
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let rules = String::from_utf8_lossy(&bytes)
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim().starts_with('#'));
        if rules {
            usable.push((path, bytes));
        }
    }
    Ok(usable)
}

/// `ls-files -i --exclude-from`: `path` or one of its parent directories is excluded by `search`.
fn excluded_by(search: &gix::ignore::Search, path: &str, case: gix::glob::pattern::Case) -> bool {
    let trimmed = path.trim_end_matches('/');
    for (at, _) in trimmed.match_indices('/') {
        let parent = &trimmed[..at];
        if search
            .pattern_matching_relative_path(parent.into(), Some(true), case)
            .is_some_and(|found| !found.pattern.is_negative())
        {
            return true;
        }
    }
    search
        .pattern_matching_relative_path(trimmed.into(), Some(path.ends_with('/')), case)
        .is_some_and(|found| !found.pattern.is_negative())
}

/// `_untracked_excluded` in process: from `entries`, the ignored listing of the status walk, the
/// paths ignored only by a source that is not a tracked path, files listed one by one, an embedded
/// repository as `dir/`. Any failure raises, so the caller reruns the fingerprint on the git CLI.
fn outside_excluded_impl(root: &str, entries: Vec<String>, info_exclude: &str) -> Res<Vec<String>> {
    let repo = open(root)?;
    let workdir = repo.workdir().ok_or("no work tree")?.to_owned();
    let workdir_real = PathBuf::from(real(&workdir)?);
    let index = repo.index_or_empty()?;
    let mut tracked = HashSet::with_capacity(index.entries().len());
    for entry in index.entries() {
        tracked.insert(text(entry.path(&index))?);
    }
    let mut stack = repo.excludes(&index, None, gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped)?;
    let mut outside = Vec::new();
    let mut tracked_dirs = Vec::new();
    let mut listed = Vec::with_capacity(entries.len());
    for path in entries {
        // The walk collapses an untracked directory holding only ignored paths to `dir/`, which
        // no rule excludes; git lists the ignored paths inside it as well.
        if let Some(dir) = path.strip_suffix('/') {
            if !excluded(&mut stack, &workdir_real, &path)? && !holds_repo(&workdir.join(dir)) {
                ignored_under(&mut stack, &workdir, &workdir_real, dir, &tracked, &mut listed)?;
            }
        }
        listed.push(path);
    }
    // `ls-files -o` drops an entry named like an index path (a tracked file replaced by a directory).
    listed.retain(|path| !tracked.contains(path.trim_end_matches('/')));
    listed.sort();
    listed.dedup();
    let info = if info_exclude.is_empty() { repo.common_dir().join("info").join("exclude") } else { PathBuf::from(info_exclude) };
    let fold = repo.config_snapshot().boolean("core.ignoreCase").unwrap_or(false);
    let case = if fold { gix::glob::pattern::Case::Fold } else { gix::glob::pattern::Case::Sensitive };
    let mut loaded: Option<Vec<(PathBuf, Vec<u8>)>> = None;
    for path in listed {
        let rule = if path.ends_with('/') {
            if loaded.is_none() {
                loaded = Some(outside_exclude_files(&repo, &workdir, &info)?);
            }
            let outside_files = loaded.as_deref().unwrap_or_default();
            dir_entry_rule(&mut stack, &workdir, &workdir_real, outside_files, &path, case)?
        } else {
            ignore_rule(&mut stack, &workdir_real, &path)?
        };
        let Some((Some(source), _)) = rule else {
            continue;
        };
        if tracked.contains(&source) {
            if path.ends_with('/') {
                tracked_dirs.push((path, source));
            }
            continue;
        }
        outside.push(path);
    }
    let mut files = Vec::new();
    for path in outside {
        let Some(dir) = path.strip_suffix('/') else {
            files.push(path);
            continue;
        };
        if holds_repo(&workdir.join(dir)) {
            files.push(path); // embedded repository: git lists the directory, not its files
            continue;
        }
        let mut inner = Vec::new();
        files_under(&workdir, dir, &tracked, &mut inner)?;
        inner.sort();
        for item in inner {
            // `ls-files -o -i -- dir` lists only the excluded paths under it.
            if let Some((Some(source), true)) = ignore_rule(&mut stack, &workdir_real, &item)? {
                if !tracked.contains(&source) {
                    files.push(item);
                }
            }
        }
    }
    // A tracked `dir/` collapse hides a later `!dir/file`; an outside exclude that matches it is hashed.
    if tracked_dirs.is_empty() {
        return Ok(files);
    }
    let sources = match loaded {
        Some(sources) => sources,
        None => outside_exclude_files(&repo, &workdir, &info)?,
    };
    if sources.is_empty() {
        return Ok(files);
    }
    let specs = negation_pathspecs(&workdir, &tracked_dirs)?;
    if specs.is_empty() {
        return Ok(files);
    }
    // Only a source that might name a negated path is asked, so the collapsed directory is walked only then.
    let sources: Vec<_> = sources
        .into_iter()
        .filter(|(_, bytes)| exclude_might_match(&String::from_utf8_lossy(bytes), &specs, fold, &workdir))
        .collect();
    if sources.is_empty() {
        return Ok(files);
    }
    let defaults = repo.pathspec_defaults()?;
    let patterns = specs
        .iter()
        .map(|spec| gix::pathspec::parse(spec.as_bytes(), defaults))
        .collect::<Result<Vec<_>, _>>()?;
    let mut pathspec = gix::pathspec::Search::from_specs(patterns, None, &workdir)?;
    let mut candidates = Vec::new();
    let mut walked = HashSet::new();
    for (dir, _) in &tracked_dirs {
        let dir = dir.trim_end_matches('/');
        if walked.insert(dir.to_owned()) {
            files_under(&workdir, dir, &tracked, &mut candidates)?;
        }
    }
    candidates.sort();
    let candidates: Vec<String> = candidates
        .into_iter()
        .filter(|path| {
            pathspec
                .pattern_matching_relative_path(path.trim_end_matches('/').into(), Some(path.ends_with('/')), &mut |_, _, _, _| false)
                .is_some_and(|found| !found.is_excluded())
        })
        .collect();
    let mut seen = HashSet::new();
    for (source, bytes) in sources {
        let mut search = gix::ignore::Search::default();
        search.add_patterns_buffer(&bytes, source, None, gix::ignore::search::Ignore { support_precious: false })?;
        for path in &candidates {
            if path.split('/').any(|part| part == "..") || seen.contains(path) || !excluded_by(&search, path, case) {
                continue;
            }
            seen.insert(path.clone());
            files.push(path.clone());
        }
    }
    Ok(files)
}

fn head_tree_impl(root: &str) -> Res<Option<String>> {
    let repo = open(root)?;
    match repo.head_tree_id() {
        Ok(id) => Ok(Some(id.to_hex().to_string())),
        Err(_) if repo.head()?.is_unborn() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A staged entry for the tree: (mode bits, object id), keyed by repository path.
type Staged = std::collections::BTreeMap<BString, (u32, gix::ObjectId)>;

const MODE_FILE: u32 = 0o100644;
const MODE_EXEC: u32 = 0o100755;
const MODE_LINK: u32 = 0o120000;
const MODE_TREE: u32 = 0o040000;

struct Writer<'a> {
    repo: &'a gix::Repository,
    store: gix::odb::loose::Store,
}

impl Writer<'_> {
    /// Hash `data` as `kind`; store it unless the repository or the store already has it.
    fn put(&self, kind: gix::objs::Kind, data: &[u8], write: bool) -> Res<gix::ObjectId> {
        use gix::objs::{Exists, Write};
        let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind, data)?;
        if write && !self.repo.objects.exists(&id) && !self.store.contains(&id) {
            self.store.write_buf(kind, data)?;
        }
        Ok(id)
    }
}

/// `git add` of one work-tree path: its index mode and blob id, or None when it is gone.
/// `write` false hashes without storing, as `update-index --info-only` does.
fn hash_path(
    w: &Writer<'_>,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    rela: &BString,
    old_mode: Option<u32>,
    trust_exec: bool,
    symlinks: bool,
    write: bool,
) -> Res<Option<(u32, gix::ObjectId)>> {
    let workdir = w.repo.workdir().ok_or("bare repository")?;
    let rela_path = gix::path::from_bstr(rela.as_bstr())?;
    // As git's has_symlink_leading_path: a parent that is a symlink or not a directory means the path is gone.
    let mut parent = workdir.to_path_buf();
    let components: Vec<_> = rela_path.components().collect();
    for component in &components[..components.len().saturating_sub(1)] {
        parent.push(component);
        match std::fs::symlink_metadata(&parent) {
            Ok(md) if md.is_dir() => {}
            _ => return Ok(None),
        }
    }
    let path = workdir.join(&rela_path);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(md) => md,
        // A parent that became a file (ENOTDIR) means the path is gone, as for git.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if md.is_symlink() {
        let target = std::fs::read_link(&path)?;
        let bytes = gix::path::into_bstr(target)?.into_owned();
        return Ok(Some((MODE_LINK, w.put(gix::objs::Kind::Blob, &bytes, write)?)));
    }
    if !md.is_file() {
        return Ok(None); // a directory now stands where the file was: git drops the entry
    }
    // As git's ce_mode_from_stat: with core.symlinks off, a regular file standing in for a tracked
    // link (how such a checkout writes it) stays a link; its content is the target.
    let mode = if !symlinks && old_mode == Some(MODE_LINK) {
        MODE_LINK
    } else if trust_exec && executable(&md) {
        MODE_EXEC
    } else if trust_exec {
        MODE_FILE
    } else {
        match old_mode {
            Some(m) if m == MODE_EXEC || m == MODE_FILE => m,
            _ => MODE_FILE,
        }
    };
    let file = std::fs::File::open(&path)?;
    let mut data = Vec::new();
    {
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        use std::io::Read;
        match pipeline.convert_to_git(file, &rela_path, index)? {
            ToGitOutcome::Unchanged(mut r) => r.read_to_end(&mut data)?,
            ToGitOutcome::Buffer(buf) => {
                data.extend_from_slice(buf);
                data.len()
            }
            ToGitOutcome::Process(mut r) => r.read_to_end(&mut data)?,
        };
    }
    Ok(Some((mode, w.put(gix::objs::Kind::Blob, &data, write)?)))
}

#[cfg(unix)]
fn executable(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o100 != 0
}

/// No executable bit to read: git keeps the index mode, as with `core.fileMode=false`.
#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// Writes the tree for `entries` (sorted full paths sharing their first `depth` components); returns its id.
fn write_tree(w: &Writer<'_>, entries: &[(&BString, (u32, gix::ObjectId))], depth: usize) -> Res<gix::ObjectId> {
    let mut tree = gix::objs::Tree::empty();
    let mut i = 0;
    while i < entries.len() {
        let rest: Vec<&[u8]> = entries[i].0.splitn(depth + 2, |b| *b == b'/').collect();
        let name = rest[depth];
        if rest.len() == depth + 1 {
            let (mode, id) = entries[i].1;
            tree.entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryMode::try_from(mode).map_err(|_| "bad mode")?,
                filename: name.into(),
                oid: id,
            });
            i += 1;
            continue;
        }
        let mut j = i;
        while j < entries.len() {
            let other: Vec<&[u8]> = entries[j].0.splitn(depth + 2, |b| *b == b'/').collect();
            if other.len() <= depth + 1 || other[depth] != name {
                break;
            }
            j += 1;
        }
        let id = write_tree(w, &entries[i..j], depth + 1)?;
        tree.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryMode::try_from(MODE_TREE).map_err(|_| "bad mode")?,
            filename: name.into(),
            oid: id,
        });
        i = j;
    }
    tree.entries.sort();
    let mut buf = Vec::new();
    gix::objs::WriteTo::write_to(&tree, &mut buf)?;
    w.put(gix::objs::Kind::Tree, &buf, true)
}

/// Untracked bytecode under `__pycache__/`, pytest's cache, and tox's `.tox/`.
/// A loose `*.pyc` stays, and so does any tracked file: this is only for
/// paths the status walk sees as untracked, or that `info_only` would hash.
/// A trailing slash stays (an embedded repository is still reported).
fn run_generated(path: &[u8]) -> bool {
    if path.ends_with(b"/") {
        return false;
    }
    let mut parts = path.split(|byte| *byte == b'/').filter(|part| !part.is_empty());
    let Some(name) = parts.next_back() else {
        return false;
    };
    let mut under_pycache = false;
    let mut under_pytest_cache = false;
    let mut under_tox = false;
    for part in parts {
        if part == b"__pycache__" {
            under_pycache = true;
        } else if part == b".pytest_cache" {
            under_pytest_cache = true;
        } else if part == b".tox" {
            under_tox = true;
        }
    }
    under_pytest_cache || under_tox || (under_pycache && name.ends_with(b".pyc"))
}

/// `add -A` (minus `exclude`) on a copy of the index, `update-index --add --info-only` for `info_only`,
/// the manifest blob at `manifest.0`, then `write-tree --missing-ok`. New objects go to `store` only.
/// `max_untracked` set means a folder outside git hashed through a private bare repository: an embedded
/// repository or more untracked files than the cap refuses, as `_nogit_refused` does.
fn stage_core(
    repo: &gix::Repository,
    store: &str,
    exclude: Vec<String>,
    info_only: Vec<String>,
    manifest: Option<(String, Vec<u8>)>,
    max_untracked: Option<usize>,
) -> Res<Staging> {
    std::fs::create_dir_all(store)?;
    let w = Writer { repo: &repo, store: gix::odb::loose::Store::at(store, gix::hash::Kind::Sha1) };
    let trust_exec = cfg!(unix) && repo.config_snapshot().boolean("core.fileMode").unwrap_or(true);
    let symlinks = repo.config_snapshot().boolean("core.symlinks").unwrap_or(true);
    let drivers = filter_drivers(repo);
    let (mut pipeline, index) = repo.filter_pipeline(None)?;
    let skipped = |path: &[u8]| {
        exclude.iter().any(|ex| {
            let ex = ex.trim_end_matches('/').as_bytes();
            path == ex || (path.starts_with(ex) && path.get(ex.len()) == Some(&b'/'))
        })
    };
    let mut staged = Staged::new();
    for entry in index.entries() {
        let path = entry.path(&index);
        if entry.stage_raw() == 0 && !entry.flags.contains(gix::index::entry::Flags::INTENT_TO_ADD) {
            staged.insert(path.to_owned(), (entry.mode.bits(), entry.id));
        }
    }
    let iter = status_platform(repo, false)?.into_index_worktree_iter(None::<BString>)?;
    let mut redo: Vec<(BString, Option<u32>)> = Vec::new();
    let mut untracked = 0usize;
    let mut embedded = false;
    for item in iter {
        match item.map_err(|e| format!("status: {e}"))? {
            WorktreeItem::Modification { rela_path, status, entry, .. } => {
                if skipped(&rela_path) {
                    continue;
                }
                match status {
                    EntryStatus::Change(Change::SubmoduleModification(_)) => {
                        return Err("a submodule differs; git add would store only its gitlink".into())
                    }
                    // Tracked files under an excluded path still count. run_generated
                    // applies to untracked entries and info-only paths, not here.
                    EntryStatus::NeedsUpdate(_) => {}
                    // git add -A drops it from the index mid-run, so later paths lose its attributes.
                    EntryStatus::Change(Change::Removed | Change::Type { .. })
                        if rela_path.rsplit_str("/").next() == Some(b".gitattributes".as_bstr()) =>
                    {
                        return Err("route: a tracked .gitattributes was removed or changed type".into())
                    }
                    _ => redo.push((rela_path, Some(entry.mode.bits()))),
                }
            }
            WorktreeItem::DirectoryContents { entry, .. } => {
                if entry.status != DirStatus::Untracked || skipped(&entry.rela_path) {
                    continue;
                }
                // A repository named like a cache is still an embedded repository.
                if !matches!(entry.disk_kind, Some(Kind::Repository)) && run_generated(entry.rela_path.as_bytes()) {
                    continue;
                }
                untracked += 1;
                // The preflight read only tracked attribute files; a new one gets the same check.
                if entry.rela_path.rsplit_str("/").next() == Some(b".gitattributes".as_bstr()) {
                    let workdir = repo.workdir().ok_or("bare repository")?;
                    let text = std::fs::read(workdir.join(gix::path::from_bstr(entry.rela_path.as_bstr())?))
                        .unwrap_or_default();
                    if let Some(reason) = unproven_attributes(&text, drivers) {
                        return Err(format!("route: a new .gitattributes uses {reason}").into());
                    }
                }
                match entry.disk_kind {
                    Some(Kind::File | Kind::Symlink) => redo.push((entry.rela_path, None)),
                    Some(Kind::Repository) if max_untracked.is_some() => embedded = true,
                    Some(Kind::Repository) => return Err("embedded git repository".into()),
                    _ => {}
                }
            }
            other => return Err(format!("unexpected status item: {other:?}").into()),
        }
    }
    if embedded {
        return Ok(Staging::Refused("embedded git repository"));
    }
    if max_untracked.is_some_and(|max| untracked > max) {
        return Ok(Staging::Refused(""));
    }
    let state: &gix::index::State = &index;
    for (path, old_mode) in redo {
        staged.remove(&path);
        let found = hash_path(&w, &mut pipeline, state, &path, old_mode, trust_exec, symlinks, true)
            .map_err(|e| format!("hash {path}: {e}"))?;
        if let Some(found) = found {
            staged.insert(path, found);
        }
    }
    for path in info_only {
        let path = BString::from(path);
        if run_generated(path.as_bytes()) {
            continue;
        }
        if let Some(found) = hash_path(&w, &mut pipeline, state, &path, None, trust_exec, symlinks, false)? {
            staged.insert(path, found);
        }
    }
    if let Some((path, content)) = manifest {
        if staged.contains_key(path.as_bytes().as_bstr()) {
            return Err("route: the manifest path is in use".into());
        }
        staged.insert(path.into(), (MODE_FILE, w.put(gix::objs::Kind::Blob, &content, true)?));
    }
    let entries: Vec<(&BString, (u32, gix::ObjectId))> = staged.iter().map(|(p, v)| (p, *v)).collect();
    Ok(Staging::Tree(write_tree(&w, &entries, 0)?.to_hex().to_string()))
}

/// A staged tree id, or why a folder outside git is refused ("" when only the file cap is passed).
enum Staging {
    Tree(String),
    Refused(&'static str),
}

fn stage_and_write_impl(
    root: &str,
    store: &str,
    exclude: Vec<String>,
    info_only: Vec<String>,
    manifest: Option<(String, Vec<u8>)>,
) -> Res<String> {
    match stage_core(&open(root)?, store, exclude, info_only, manifest, None)? {
        Staging::Tree(id) => Ok(id),
        Staging::Refused(reason) => Err(reason.into()),
    }
}

/// The no-git fingerprint: `root` hashed through a new private repository at `git_dir` whose config
/// points its work tree at `root` (the CLI path sets GIT_WORK_TREE instead), with an empty index.
/// Returns (tree, refusal reason).
fn nogit_impl(root: &str, git_dir: &str, store: &str, max_files: usize) -> Res<(Option<String>, Option<String>)> {
    use std::io::Write;
    if Path::new(git_dir).exists() {
        return Err("the private repository must be new".into());
    }
    let root = plain(std::fs::canonicalize(root)?);
    let root = root.to_str().ok_or("path is not UTF-8")?;
    if root.contains('\n') {
        return Err("route: a newline in the folder path".into());
    }
    gix::init_bare(git_dir)?;
    let quoted = root.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config = std::fs::OpenOptions::new().append(true).open(Path::new(git_dir).join("config"))?;
    writeln!(config, "[core]\n\tbare = false\n\tworktree = \"{quoted}\"")?;
    drop(config);
    let repo = gix::open(git_dir)?;
    if repo.workdir() != Some(Path::new(root)) {
        return Err(format!("the private repository did not take the work tree: {:?}", repo.workdir()).into());
    }
    Ok(match stage_core(&repo, store, Vec::new(), Vec::new(), None, Some(max_files))? {
        Staging::Tree(id) => (Some(id), None),
        Staging::Refused(reason) => (None, Some(reason.to_owned())),
    })
}

#[pyfunction]
#[pyo3(signature = (root, store, exclude, info_only, manifest=None))]
fn stage_and_write(
    py: Python<'_>,
    root: String,
    store: String,
    exclude: Vec<String>,
    info_only: Vec<String>,
    manifest: Option<(String, Vec<u8>)>,
) -> PyResult<String> {
    py.detach(|| stage_and_write_impl(&root, &store, exclude, info_only, manifest)).map_err(err)
}

/// Environment variables that move git's view of the repository; the git CLI honors them all.
/// (GIT_CONFIG_GLOBAL, GIT_CONFIG_SYSTEM, and GIT_CONFIG_COUNT are absent: gix reads them as git does.)
const RELOCATING_ENV: &[&str] = &[
    "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR", "GIT_NAMESPACE", "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG", "GIT_CONFIG_PARAMETERS",
    "GIT_ATTR_SOURCE", "GIT_ICASE_PATHSPECS", "GIT_LITERAL_PATHSPECS", "GIT_GLOB_PATHSPECS",
];

/// `extensions.*` keys this engine reads like git; any other routes to the CLI.
const KNOWN_EXTENSIONS: &[&str] = &["objectformat", "refstorage", "worktreeconfig", "preciousobjects", "noop"];

/// Whether any `filter.<driver>` has a command git would run.
fn filter_drivers(repo: &gix::Repository) -> bool {
    repo.config_snapshot()
        .plumbing()
        .sections_by_name("filter")
        .into_iter()
        .flatten()
        .any(|s| ["clean", "smudge", "process"].iter().any(|k| s.value(k).is_some()))
}

/// Bytes of an attribute line this engine parses itself: printable ASCII except a quote or a backslash, or a blank.
fn plain_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | 0x21..=0x7e) && byte != b'"' && byte != b'\\'
}

/// The conversion attribute text names that this engine has not proven equal to git's, if any.
/// Blank lines and `#` comments are skipped. A line of only plain bytes (no quote, backslash, NUL, BOM or
/// other non-ASCII) is parsed: its first word is the pattern, later words are attribute names (a leading
/// `-` or `!` and any `=value` dropped). Any other line is matched as a raw substring, which covers every
/// git quirk. A false hit only sends the call to the git CLI.
fn unproven_attributes(text: &[u8], drivers: bool) -> Option<&'static str> {
    let (mut encoding, mut ident, mut filter) = (false, false, false);
    let blank = |b: &u8| matches!(b, b' ' | b'\t' | b'\r');
    for line in text.split(|&b| b == b'\n') {
        let line = &line[line.iter().position(|b| !blank(b)).unwrap_or(line.len())..];
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        if !line.iter().all(|&b| plain_byte(b)) {
            encoding |= line.find("working-tree-encoding").is_some();
            ident |= line.find("ident").is_some();
            filter |= line.find("filter").is_some();
            continue;
        }
        for word in line.split(blank).skip(1) {
            let word = word.strip_prefix(b"-").or_else(|| word.strip_prefix(b"!")).unwrap_or(word);
            let name = word.split(|&b| b == b'=').next().unwrap_or(word);
            encoding |= name == b"working-tree-encoding";
            ident |= name == b"ident";
            filter |= name == b"filter";
        }
    }
    if encoding {
        Some("working-tree-encoding attribute")
    } else if ident {
        Some("ident attribute")
    } else if drivers && filter {
        Some("a filter driver an attribute uses")
    } else {
        None
    }
}

/// Whether this repository may be fingerprinted in process: ("inprocess", "") or ("cli", reason).
/// `nogit`: a folder outside any repository, hashed through a private one; only the environment matters.
fn preflight_impl(root: &str, max_entries: usize, nogit: bool) -> Res<(String, String)> {
    let cli = |reason: &str| Ok(("cli".to_owned(), reason.to_owned()));
    for name in RELOCATING_ENV {
        if std::env::var_os(name).is_some() {
            return cli(&format!("{name} is set"));
        }
    }
    if nogit {
        return Ok(("inprocess".to_owned(), String::new()));
    }
    let repo = match gix::discover(root) {
        Ok(repo) => repo,
        Err(_) => return cli("not a repository gix can open"),
    };
    let config = repo.config_snapshot();
    if config.integer("core.repositoryFormatVersion").unwrap_or(0) > 1 {
        return cli("core.repositoryFormatVersion above 1");
    }
    let file = config.plumbing();
    for section in file.sections_by_name("extensions").into_iter().flatten() {
        for key in section.body().value_names() {
            let key = key.to_ascii_lowercase();
            if !KNOWN_EXTENSIONS.contains(&key.as_str()) {
                return cli(&format!("extensions.{key}"));
            }
        }
    }
    if config.string("extensions.objectFormat").is_some_and(|v| !v.eq_ignore_ascii_case(b"sha1")) {
        return cli("extensions.objectFormat is not sha1");
    }
    if config.string("extensions.refStorage").is_some_and(|v| !v.eq_ignore_ascii_case(b"files")) {
        return cli("extensions.refStorage is not files");
    }
    if let Some(value) = config.string("core.fsmonitor") {
        if config.boolean("core.fsmonitor") != Some(false) && !value.is_empty() {
            return cli("core.fsmonitor is set");
        }
    }
    if config.boolean("core.safecrlf") == Some(true) {
        return cli("core.safecrlf is true");
    }
    if config.string("core.bigFileThreshold").is_some() {
        return cli("core.bigFileThreshold packs large blobs");
    }
    // Failed parity: with core.ignoreCase on a case-sensitive filesystem, gix and git disagree on excludes.
    if config.boolean("core.ignoreCase") == Some(true) && !case_insensitive_fs(repo.git_dir()) {
        return cli("core.ignoreCase on a case-sensitive filesystem");
    }
    // Failed parity: git reads a relative excludesFile from the work tree, gix does not.
    if matches!(config.trusted_path("core.excludesFile"), Ok(Some(path)) if !path.is_absolute()) {
        return cli("core.excludesFile is relative");
    }
    let index = repo.index_or_empty()?;
    if index.is_sparse() {
        return cli("sparse index");
    }
    if index.entries().len() > max_entries {
        return cli("index above the in-process size threshold");
    }
    // Attribute sources: tracked .gitattributes (work tree copy), info/attributes, the global file.
    let workdir = repo.workdir().ok_or("bare repository")?;
    let mut sources: Vec<PathBuf> = index
        .entries()
        .iter()
        .map(|e| e.path(&index))
        .filter(|p| p.rsplit_str("/").next() == Some(b".gitattributes".as_bstr()))
        .filter_map(|p| gix::path::from_bstr(p).ok().map(|p| workdir.join(p)))
        .collect();
    sources.push(repo.common_dir().join("info").join("attributes"));
    if let Ok(Some(global)) = config.trusted_path("core.attributesFile") {
        sources.push(global.into());
    } else if let Some(base) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    }) {
        sources.push(base.join("git").join("attributes"));
    }
    let drivers = filter_drivers(&repo);
    for source in sources {
        let Ok(text) = std::fs::read(&source) else { continue };
        if let Some(reason) = unproven_attributes(&text, drivers) {
            return cli(reason);
        }
    }
    Ok(("inprocess".to_owned(), String::new()))
}

#[pyfunction]
#[pyo3(signature = (root, max_entries, nogit=false))]
fn preflight(py: Python<'_>, root: String, max_entries: usize, nogit: bool) -> PyResult<(String, String)> {
    py.detach(|| preflight_impl(&root, max_entries, nogit)).map_err(err)
}

#[pyfunction]
fn nogit_stage_and_write(
    py: Python<'_>,
    root: String,
    git_dir: String,
    store: String,
    max_files: usize,
) -> PyResult<(Option<String>, Option<String>)> {
    py.detach(|| nogit_impl(&root, &git_dir, &store, max_files)).map_err(err)
}

#[pyfunction]
fn paths(py: Python<'_>, cwd: String) -> PyResult<(String, String, String, String, String)> {
    py.detach(|| paths_impl(&cwd)).map_err(err)
}

#[pyfunction]
fn linked_worktrees(py: Python<'_>, root: String) -> PyResult<Vec<String>> {
    py.detach(|| linked_worktrees_impl(&root)).map_err(err)
}

#[pyfunction]
#[pyo3(signature = (root, threads=None))]
fn status(py: Python<'_>, root: String, threads: Option<usize>) -> PyResult<(bool, Vec<String>, Vec<SubmoduleFacts>)> {
    py.detach(|| status_impl(&root, threads)).map_err(err)
}

#[pyfunction]
fn status_and_ignored(py: Python<'_>, root: String) -> PyResult<(StatusFacts, Vec<String>)> {
    py.detach(|| status_walk(&root, None, true)).map_err(err)
}

#[pyfunction]
fn tracked_special(py: Python<'_>, root: String) -> PyResult<Vec<Tracked>> {
    py.detach(|| tracked_special_impl(&root)).map_err(err)
}

#[pyfunction]
fn tracked(py: Python<'_>, root: String) -> PyResult<Vec<Tracked>> {
    py.detach(|| tracked_impl(&root)).map_err(err)
}

#[pyfunction]
fn ignored_untracked(py: Python<'_>, root: String) -> PyResult<Vec<String>> {
    py.detach(|| ignored_untracked_impl(&root)).map_err(err)
}

#[pyfunction]
fn outside_excluded(py: Python<'_>, root: String, entries: Vec<String>, info_exclude: String) -> PyResult<Vec<String>> {
    py.detach(|| outside_excluded_impl(&root, entries, &info_exclude)).map_err(err)
}

#[pyfunction]
fn head_tree(py: Python<'_>, root: String) -> PyResult<Option<String>> {
    py.detach(|| head_tree_impl(&root)).map_err(err)
}

#[pymodule]
fn sureforge_gix(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("ENGINE_API", 2)?;
    m.add_function(wrap_pyfunction!(preflight, m)?)?;
    m.add_function(wrap_pyfunction!(nogit_stage_and_write, m)?)?;
    m.add_function(wrap_pyfunction!(paths, m)?)?;
    m.add_function(wrap_pyfunction!(linked_worktrees, m)?)?;
    m.add_function(wrap_pyfunction!(status, m)?)?;
    m.add_function(wrap_pyfunction!(tracked, m)?)?;
    m.add_function(wrap_pyfunction!(tracked_special, m)?)?;
    m.add_function(wrap_pyfunction!(status_and_ignored, m)?)?;
    m.add_function(wrap_pyfunction!(ignored_untracked, m)?)?;
    m.add_function(wrap_pyfunction!(outside_excluded, m)?)?;
    m.add_function(wrap_pyfunction!(head_tree, m)?)?;
    m.add_function(wrap_pyfunction!(stage_and_write, m)?)?;
    Ok(())
}
