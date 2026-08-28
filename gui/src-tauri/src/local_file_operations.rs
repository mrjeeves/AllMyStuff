//! Collision-safe, bounded local file operations for the Files workspace.
//!
//! The webview, context menus, keyboard shortcuts, and canvas drops all call
//! this one journal. A multi-item move rolls back if any rename fails; copies
//! are built under a hidden staging directory and become visible only after
//! the complete selection succeeds. Undo refuses to remove a copied item that
//! has changed since this journal created it.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

const HISTORY_LIMIT: usize = 50;
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LocalFileOperationKind {
    Copy,
    Move,
}

#[derive(Clone, Debug)]
struct Pair {
    from: PathBuf,
    to: PathBuf,
}

#[derive(Clone, Debug)]
struct JournalEntry {
    kind: LocalFileOperationKind,
    pairs: Vec<Pair>,
    copied_signatures: Vec<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalFileOperationResult {
    pub operation: String,
    pub paths: Vec<String>,
    pub affected: usize,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Default)]
pub struct LocalFileOperations {
    undo: Vec<JournalEntry>,
    redo: Vec<JournalEntry>,
}

impl LocalFileOperations {
    pub fn state(&self) -> LocalFileOperationResult {
        LocalFileOperationResult {
            operation: String::new(),
            paths: Vec::new(),
            affected: 0,
            can_undo: !self.undo.is_empty(),
            can_redo: !self.redo.is_empty(),
        }
    }

    pub fn apply(
        &mut self,
        paths: Vec<String>,
        destination: String,
        kind: LocalFileOperationKind,
    ) -> Result<LocalFileOperationResult, String> {
        let pairs = plan_pairs(paths, &destination)?;
        let copied_signatures = execute(kind, &pairs)?;
        let result_paths = pairs
            .iter()
            .map(|pair| pair.to.to_string_lossy().into_owned())
            .collect();
        let affected = pairs.len();
        self.undo.push(JournalEntry {
            kind,
            pairs,
            copied_signatures,
        });
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        Ok(LocalFileOperationResult {
            operation: match kind {
                LocalFileOperationKind::Copy => "Copied",
                LocalFileOperationKind::Move => "Moved",
            }
            .into(),
            paths: result_paths,
            affected,
            can_undo: true,
            can_redo: false,
        })
    }

    pub fn undo(&mut self) -> Result<LocalFileOperationResult, String> {
        let entry = self.undo.last().cloned().ok_or("nothing to undo")?;
        match entry.kind {
            LocalFileOperationKind::Move => {
                let inverse: Vec<Pair> = entry
                    .pairs
                    .iter()
                    .map(|pair| Pair {
                        from: pair.to.clone(),
                        to: pair.from.clone(),
                    })
                    .collect();
                execute_move(&inverse)?;
            }
            LocalFileOperationKind::Copy => {
                for (pair, expected) in entry.pairs.iter().zip(&entry.copied_signatures) {
                    if !pair.to.exists() {
                        return Err(format!(
                            "can't undo because {} no longer exists",
                            pair.to.to_string_lossy()
                        ));
                    }
                    if tree_signature(&pair.to)? != *expected {
                        return Err(format!(
                            "can't undo because {} changed after it was copied",
                            pair.to.to_string_lossy()
                        ));
                    }
                }
                for pair in entry.pairs.iter().rev() {
                    remove_tree(&pair.to)?;
                }
            }
        }
        self.undo.pop();
        self.redo.push(entry.clone());
        Ok(LocalFileOperationResult {
            operation: format!(
                "Undid {}",
                match entry.kind {
                    LocalFileOperationKind::Copy => "copy",
                    LocalFileOperationKind::Move => "move",
                }
            ),
            paths: entry
                .pairs
                .iter()
                .map(|pair| pair.from.to_string_lossy().into_owned())
                .collect(),
            affected: entry.pairs.len(),
            can_undo: !self.undo.is_empty(),
            can_redo: true,
        })
    }

    pub fn redo(&mut self) -> Result<LocalFileOperationResult, String> {
        let mut entry = self.redo.last().cloned().ok_or("nothing to redo")?;
        entry.copied_signatures = execute(entry.kind, &entry.pairs)?;
        self.redo.pop();
        self.undo.push(entry.clone());
        Ok(LocalFileOperationResult {
            operation: format!(
                "Redid {}",
                match entry.kind {
                    LocalFileOperationKind::Copy => "copy",
                    LocalFileOperationKind::Move => "move",
                }
            ),
            paths: entry
                .pairs
                .iter()
                .map(|pair| pair.to.to_string_lossy().into_owned())
                .collect(),
            affected: entry.pairs.len(),
            can_undo: true,
            can_redo: !self.redo.is_empty(),
        })
    }
}

fn plan_pairs(paths: Vec<String>, destination: &str) -> Result<Vec<Pair>, String> {
    if paths.is_empty() {
        return Err("select at least one file or folder".into());
    }
    let destination = PathBuf::from(destination);
    if !destination.is_dir() {
        return Err("the destination is not an available folder".into());
    }
    let destination = destination
        .canonicalize()
        .map_err(|error| format!("can't open destination: {error}"))?;
    let mut seen_sources = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut pairs = Vec::with_capacity(paths.len());
    for raw in paths {
        let source = PathBuf::from(raw);
        let source = source
            .canonicalize()
            .map_err(|error| format!("can't open {}: {error}", source.to_string_lossy()))?;
        if !seen_sources.insert(source.clone()) {
            continue;
        }
        let name = source
            .file_name()
            .ok_or_else(|| format!("{} has no file name", source.to_string_lossy()))?;
        let folded = name.to_string_lossy().to_lowercase();
        if !seen_names.insert(folded) {
            return Err(format!(
                "more than one selected item is named {}",
                name.to_string_lossy()
            ));
        }
        if source.parent() == Some(destination.as_path()) {
            return Err(format!(
                "{} is already in that folder",
                name.to_string_lossy()
            ));
        }
        if source.is_dir() && destination.starts_with(&source) {
            return Err(format!(
                "can't place {} inside itself",
                source.to_string_lossy()
            ));
        }
        let target = destination.join(name);
        if target.exists() {
            return Err(format!(
                "{} already contains {}",
                destination.to_string_lossy(),
                name.to_string_lossy()
            ));
        }
        pairs.push(Pair {
            from: source,
            to: target,
        });
    }
    if pairs.is_empty() {
        return Err("select at least one file or folder".into());
    }
    Ok(pairs)
}

fn execute(kind: LocalFileOperationKind, pairs: &[Pair]) -> Result<Vec<u64>, String> {
    match kind {
        LocalFileOperationKind::Move => {
            execute_move(pairs)?;
            Ok(Vec::new())
        }
        LocalFileOperationKind::Copy => execute_copy(pairs),
    }
}

fn execute_move(pairs: &[Pair]) -> Result<(), String> {
    for pair in pairs {
        if !pair.from.exists() {
            return Err(format!("{} no longer exists", pair.from.to_string_lossy()));
        }
        if pair.to.exists() {
            return Err(format!("{} already exists", pair.to.to_string_lossy()));
        }
    }
    let mut completed: Vec<Pair> = Vec::new();
    for pair in pairs {
        if let Err(error) = fs::rename(&pair.from, &pair.to) {
            let mut rollback_errors = Vec::new();
            for done in completed.iter().rev() {
                if let Err(rollback) = fs::rename(&done.to, &done.from) {
                    rollback_errors.push(rollback.to_string());
                }
            }
            let rollback = if rollback_errors.is_empty() {
                String::new()
            } else {
                format!("; rollback also failed: {}", rollback_errors.join("; "))
            };
            return Err(format!(
                "couldn't move {}: {error}{rollback}",
                pair.from.to_string_lossy()
            ));
        }
        completed.push(pair.clone());
    }
    Ok(())
}

fn execute_copy(pairs: &[Pair]) -> Result<Vec<u64>, String> {
    for pair in pairs {
        if !pair.from.exists() {
            return Err(format!("{} no longer exists", pair.from.to_string_lossy()));
        }
        if pair.to.exists() {
            return Err(format!("{} already exists", pair.to.to_string_lossy()));
        }
    }
    let destination = pairs[0]
        .to
        .parent()
        .ok_or("the destination has no parent folder")?;
    let stage = destination.join(format!(
        ".allmystuff-operation-{}-{}",
        std::process::id(),
        NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&stage).map_err(|error| format!("can't stage copy: {error}"))?;
    let staged: Vec<Pair> = pairs
        .iter()
        .map(|pair| Pair {
            from: pair.from.clone(),
            to: stage.join(pair.to.file_name().expect("planned target has a name")),
        })
        .collect();
    let copy_result = staged
        .iter()
        .try_for_each(|pair| copy_tree(&pair.from, &pair.to));
    if let Err(error) = copy_result {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let mut committed: Vec<Pair> = Vec::new();
    for (staged_pair, final_pair) in staged.iter().zip(pairs) {
        if let Err(error) = fs::rename(&staged_pair.to, &final_pair.to) {
            for done in committed.iter().rev() {
                let staged_back = stage.join(done.to.file_name().expect("target has a name"));
                let _ = fs::rename(&done.to, staged_back);
            }
            let _ = fs::remove_dir_all(&stage);
            return Err(format!(
                "couldn't commit {}: {error}",
                final_pair.to.to_string_lossy()
            ));
        }
        committed.push(final_pair.clone());
    }
    let _ = fs::remove_dir(&stage);
    pairs.iter().map(|pair| tree_signature(&pair.to)).collect()
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(from)
        .map_err(|error| format!("can't inspect {}: {error}", from.to_string_lossy()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "symbolic links need an explicit copy policy: {}",
            from.to_string_lossy()
        ));
    }
    if metadata.is_dir() {
        fs::create_dir(to)
            .map_err(|error| format!("can't create {}: {error}", to.to_string_lossy()))?;
        let children = fs::read_dir(from)
            .map_err(|error| format!("can't read {}: {error}", from.to_string_lossy()))?;
        for child in children {
            let child =
                child.map_err(|error| format!("can't read {}: {error}", from.to_string_lossy()))?;
            copy_tree(&child.path(), &to.join(child.file_name()))?;
        }
        fs::set_permissions(to, metadata.permissions())
            .map_err(|error| format!("can't preserve permissions: {error}"))?;
        Ok(())
    } else if metadata.is_file() {
        fs::copy(from, to)
            .map(|_| ())
            .map_err(|error| format!("can't copy {}: {error}", from.to_string_lossy()))
    } else {
        Err(format!("unsupported file kind: {}", from.to_string_lossy()))
    }
}

fn remove_tree(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| format!("can't undo {}: {error}", path.to_string_lossy()))
}

fn tree_signature(path: &Path) -> Result<u64, String> {
    fn visit(path: &Path, root: &Path) -> Result<u64, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("can't inspect {}: {error}", path.to_string_lossy()))?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.strip_prefix(root).unwrap_or(path).hash(&mut hasher);
        metadata.file_type().is_symlink().hash(&mut hasher);
        metadata.is_dir().hash(&mut hasher);
        metadata.len().hash(&mut hasher);
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .hash(&mut hasher);
        if metadata.is_dir() {
            let children = fs::read_dir(path)
                .map_err(|error| format!("can't read {}: {error}", path.to_string_lossy()))?;
            let mut child_count = 0u64;
            let mut child_xor = 0u64;
            let mut child_sum = 0u64;
            for child in children {
                let child = child
                    .map_err(|error| format!("can't read {}: {error}", path.to_string_lossy()))?;
                let signature = visit(&child.path(), root)?;
                child_count = child_count.wrapping_add(1);
                child_xor ^= signature.rotate_left((signature & 63) as u32);
                child_sum = child_sum.wrapping_add(signature.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            }
            child_count.hash(&mut hasher);
            child_xor.hash(&mut hasher);
            child_sum.hash(&mut hasher);
        }
        Ok(hasher.finish())
    }
    visit(path, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "allmystuff-local-ops-{name}-{}-{}",
            std::process::id(),
            NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn multi_move_undo_redo_is_one_operation() {
        let root = temp("move");
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("one.txt"), b"one").unwrap();
        fs::write(source.join("two.txt"), b"two").unwrap();
        let mut history = LocalFileOperations::default();
        history
            .apply(
                vec![
                    source.join("one.txt").to_string_lossy().into_owned(),
                    source.join("two.txt").to_string_lossy().into_owned(),
                ],
                destination.to_string_lossy().into_owned(),
                LocalFileOperationKind::Move,
            )
            .unwrap();
        assert!(destination.join("one.txt").exists());
        history.undo().unwrap();
        assert!(source.join("one.txt").exists());
        history.redo().unwrap();
        assert!(destination.join("two.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_undo_refuses_to_delete_changed_data() {
        let root = temp("copy");
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("one.txt"), b"one").unwrap();
        let mut history = LocalFileOperations::default();
        history
            .apply(
                vec![source.join("one.txt").to_string_lossy().into_owned()],
                destination.to_string_lossy().into_owned(),
                LocalFileOperationKind::Copy,
            )
            .unwrap();
        fs::write(destination.join("one.txt"), b"changed").unwrap();
        assert!(history.undo().unwrap_err().contains("changed after"));
        assert_eq!(fs::read(destination.join("one.txt")).unwrap(), b"changed");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collision_preflight_moves_nothing() {
        let root = temp("collision");
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("one.txt"), b"source").unwrap();
        fs::write(destination.join("one.txt"), b"destination").unwrap();
        let mut history = LocalFileOperations::default();
        assert!(history
            .apply(
                vec![source.join("one.txt").to_string_lossy().into_owned()],
                destination.to_string_lossy().into_owned(),
                LocalFileOperationKind::Move,
            )
            .unwrap_err()
            .contains("already contains"));
        assert!(source.join("one.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
