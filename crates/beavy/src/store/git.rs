//! What beavy reads and writes in a Git working tree, without running git: the
//! commit a snapshot was taken at, the states it refuses to import, and the
//! detached HEAD it leaves in the imported copy.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};

use super::at;
use crate::error::{Error, Next, Reason};

/// Refuse to import a directory whose Git state a copy would get wrong: a linked
/// worktree or submodule, whose repository lives outside it, or a repository in
/// the middle of a merge, rebase, cherry-pick, revert, or bisect, or locked by a
/// running git.
pub(crate) fn check(directory: &Path) -> Result<(), Error> {
    let dot_git = directory.join(".git");
    let shown = directory.display();
    let metadata = match fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(super::fs_error(err, format!("read {}", dot_git.display()))),
    };
    if !metadata.is_dir() {
        let error = Error::new(
            Reason::UnsupportedGitState,
            format!(
                "Can't import {shown}. It's a linked Git worktree or submodule, so its repository lives outside it."
            ),
        );
        return Err(match main_worktree(&dot_git) {
            Some(main) => error.with_next(
                "Import the main working tree instead",
                Next::new([
                    "beavy".to_string(),
                    "snapshots".into(),
                    "create".into(),
                    "--cwd".into(),
                    main.to_string_lossy().into_owned(),
                ]),
            ),
            None => error,
        });
    }
    let states = [
        ("MERGE_HEAD", "a merge is in progress"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
        ("BISECT_LOG", "a bisect is in progress"),
        ("rebase-merge", "a rebase is in progress"),
        ("rebase-apply", "a rebase or am is in progress"),
        ("index.lock", "a git process holds its index lock"),
        ("HEAD.lock", "a git process holds its HEAD lock"),
    ];
    for (file, state) in states {
        if dot_git.join(file).exists() {
            return Err(Error::new(
                Reason::UnsupportedGitState,
                format!(
                    "Can't import {shown}. In its Git repository, {state} (.git/{file}). Finish it or abort it, then import again."
                ),
            ));
        }
    }
    Ok(())
}

/// The main working tree of a linked worktree, from the gitdir its .git file names:
/// `/w/acme/.git/worktrees/topic` belongs to `/w/acme`.
fn main_worktree(dot_git: &Path) -> Option<PathBuf> {
    let pointer = fs::read_to_string(dot_git).ok()?;
    let git_dir = dot_git
        .parent()?
        .join(pointer.trim().strip_prefix("gitdir: ")?);
    let worktrees = git_dir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let common = worktrees.parent()?;
    (common.file_name()? == ".git").then(|| common.parent().map(Path::to_path_buf))?
}

/// The commit checked out in a working tree. `read` returns the text of a path
/// relative to the tree's root; a path from a .git file that points elsewhere
/// arrives as it's written there.
pub(crate) fn head(read: impl Fn(&Path) -> Option<String>) -> Option<String> {
    let git_dir = match read(Path::new(".git/HEAD")) {
        Some(_) => PathBuf::from(".git"),
        None => PathBuf::from(read(Path::new(".git"))?.trim().strip_prefix("gitdir: ")?),
    };
    let common = read(&git_dir.join("commondir"))
        .map(|dir| git_dir.join(dir.trim()))
        .unwrap_or_else(|| git_dir.clone());
    let head = read(&git_dir.join("HEAD"))?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref: ") else {
        return is_hash(head).then(|| head.to_owned());
    };
    for base in [&git_dir, &common] {
        if let Some(value) = read(&base.join(reference)) {
            let value = value.trim();
            if is_hash(value) {
                return Some(value.to_owned());
            }
        }
    }
    read(&common.join("packed-refs"))?.lines().find_map(|line| {
        let (hash, name) = line.split_once(' ')?;
        (name == reference && is_hash(hash)).then(|| hash.to_owned())
    })
}

/// A reader for [`head`] that reads regular files beneath the open directory
/// `tree` and nothing else: no symlinks, and no paths that leave it.
pub(crate) fn reader(tree: &File) -> impl Fn(&Path) -> Option<String> + '_ {
    |relative| read_beneath(tree, relative).ok()
}

fn read_beneath(tree: &File, relative: &Path) -> io::Result<String> {
    let fd = at::beneath(
        tree,
        relative,
        OFlags::RDONLY | OFlags::NOCTTY | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    if at::fstat(&fd)?.kind != FileType::RegularFile {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut text = String::new();
    File::from(fd).read_to_string(&mut text)?;
    Ok(text)
}

/// Point the copy's HEAD at `commit` directly, so work in a worktree never moves
/// a branch the source has checked out. `tree` is the copy, open. Leaves a HEAD
/// that's already detached, or that isn't a regular file. The new HEAD replaces the
/// old one with a rename, so nothing is written through a link.
pub(crate) fn detach(tree: &File, commit: &str) -> io::Result<()> {
    let git = match at::open_dir_beneath(tree, Path::new(".git")) {
        Ok(git) => git,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let git = File::from(git);
    match read_beneath(&git, Path::new("HEAD")) {
        Ok(text) if text.starts_with("ref: ") => {}
        _ => return Ok(()),
    }
    let temporary = format!("HEAD.beavy-{}", super::random_id());
    let write = || -> io::Result<()> {
        let mut file = File::from(rustix::fs::openat(
            &git,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )?);
        file.write_all(format!("{commit}\n").as_bytes())?;
        rustix::fs::renameat(&git, temporary.as_str(), &git, "HEAD")?;
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&git, temporary.as_str(), rustix::fs::AtFlags::empty());
    }
    result
}

fn is_hash(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read_from(files: &[(&str, &str)]) -> impl Fn(&Path) -> Option<String> + use<> {
        let files: HashMap<PathBuf, String> = files
            .iter()
            .map(|(path, text)| (PathBuf::from(path), text.to_string()))
            .collect();
        move |path| files.get(path).cloned()
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn head_follows_refs_packed_refs_and_detached_heads() {
        let loose = read_from(&[
            (".git/HEAD", "ref: refs/heads/main\n"),
            (".git/refs/heads/main", HASH),
        ]);
        assert_eq!(head(loose).as_deref(), Some(HASH));
        let packed = format!("# pack-refs\n{HASH} refs/heads/main\n");
        let packed = read_from(&[
            (".git/HEAD", "ref: refs/heads/main"),
            (".git/packed-refs", &packed),
        ]);
        assert_eq!(head(packed).as_deref(), Some(HASH));
        assert_eq!(
            head(read_from(&[(".git/HEAD", HASH)])).as_deref(),
            Some(HASH)
        );
        let linked = read_from(&[
            (".git", "gitdir: /repo/.git/worktrees/w\n"),
            ("/repo/.git/worktrees/w/HEAD", "ref: refs/heads/topic"),
            ("/repo/.git/worktrees/w/commondir", "../.."),
            ("/repo/.git/worktrees/w/../../refs/heads/topic", HASH),
        ]);
        assert_eq!(head(linked).as_deref(), Some(HASH));
        assert_eq!(head(read_from(&[])), None);
        assert_eq!(
            head(read_from(&[(".git/HEAD", "ref: refs/heads/none")])),
            None
        );
    }

    #[test]
    fn check_refuses_linked_worktrees_and_operations_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        check(root).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        check(root).unwrap();
        fs::write(root.join(".git/MERGE_HEAD"), HASH).unwrap();
        let err = check(root).unwrap_err();
        assert_eq!(err.reason(), Reason::UnsupportedGitState);
        assert!(
            err.message().contains("a merge is in progress"),
            "{}",
            err.message()
        );

        let linked = root.join("linked");
        fs::create_dir(&linked).unwrap();
        let gitdir = root.join("main/.git/worktrees/linked");
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        let err = check(&linked).unwrap_err();
        assert_eq!(err.reason(), Reason::UnsupportedGitState);
        assert_eq!(
            err.next().unwrap().argv[4],
            root.join("main").to_str().unwrap()
        );
    }

    #[test]
    fn detach_rewrites_only_a_symbolic_head() {
        let dir = tempfile::tempdir().unwrap();
        let tree = File::from(at::open_dir(dir.path()).unwrap());
        detach(&tree, HASH).unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        detach(&tree, HASH).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".git/HEAD")).unwrap(),
            format!("{HASH}\n")
        );
        assert_eq!(fs::read_dir(dir.path().join(".git")).unwrap().count(), 1);
    }

    #[test]
    fn detach_and_head_never_follow_a_symlinked_head() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let copy = dir.path().join("copy");
        fs::create_dir_all(copy.join(".git/refs/heads")).unwrap();
        fs::write(copy.join(".git/refs/heads/main"), HASH).unwrap();
        std::os::unix::fs::symlink(other.join("HEAD"), copy.join(".git/HEAD")).unwrap();
        let tree = File::from(at::open_dir(&copy).unwrap());
        assert_eq!(head(reader(&tree)), None);
        detach(&tree, HASH).unwrap();
        assert_eq!(
            fs::read_to_string(other.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
    }
}
