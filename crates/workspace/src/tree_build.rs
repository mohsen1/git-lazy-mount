//! Build a new tree from a base tree plus a delta, reusing unchanged subtrees
//! (spec §24 steps 1–4, §49 "rename clean subtree ~ O(changed mappings)").
//!
//! Only directories that actually contain a change are recursed into; untouched
//! subtrees keep their existing object id verbatim (no fetch, no rewrite).

use std::collections::BTreeMap;

use glm_core::{FetchPolicy, GitMode, ObjectId, RepoPath, Result, TreeEntry};
use glm_git_store::GitStore;
use glm_object_provider::ObjectProvider;

/// A path decomposed into components, paired with the change to apply there.
type DecomposedChange = (Vec<Vec<u8>>, TreeChange);

/// A change to apply to a tree at a path.
#[derive(Clone, Debug)]
pub enum TreeChange {
    /// Set the entry to a blob/subtree with a mode.
    Set {
        /// Object id of the new content.
        oid: ObjectId,
        /// Mode for the entry.
        mode: GitMode,
    },
    /// Remove the entry.
    Remove,
}

/// Build a tree object from `base_tree` with `changes` applied. Returns the new
/// tree's object id (written to the store).
pub fn build_tree(
    store: &GitStore,
    provider: &dyn ObjectProvider,
    base_tree: Option<ObjectId>,
    changes: Vec<(RepoPath, TreeChange)>,
    policy: FetchPolicy,
) -> Result<ObjectId> {
    let decomposed: Vec<DecomposedChange> = changes
        .into_iter()
        .map(|(p, c)| (p.components().map(|c| c.to_vec()).collect(), c))
        .collect();
    let (oid, _count) = recurse(store, provider, base_tree, decomposed, policy)?;
    Ok(oid)
}

fn recurse(
    store: &GitStore,
    provider: &dyn ObjectProvider,
    base_tree: Option<ObjectId>,
    changes: Vec<DecomposedChange>,
    policy: FetchPolicy,
) -> Result<(ObjectId, usize)> {
    // Start from the base directory's entries (if any).
    let mut entries: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
    if let Some(base) = &base_tree {
        let tree = provider.tree(base, policy)?;
        for e in tree.entries {
            entries.insert(e.name.clone(), e);
        }
    }

    // Partition into direct (this dir) and nested (subdirectory) changes.
    let mut direct: Vec<(Vec<u8>, TreeChange)> = Vec::new();
    let mut nested: BTreeMap<Vec<u8>, Vec<DecomposedChange>> = BTreeMap::new();
    for (comps, ch) in changes {
        if comps.len() == 1 {
            direct.push((comps.into_iter().next().unwrap(), ch));
        } else if let Some((head, rest)) = comps.split_first() {
            nested
                .entry(head.clone())
                .or_default()
                .push((rest.to_vec(), ch));
        }
    }

    // Recurse into changed subdirectories only.
    for (name, sub_changes) in nested {
        let base_sub = entries
            .get(&name)
            .filter(|e| matches!(e.mode, GitMode::Tree))
            .map(|e| e.object_id.clone());
        let (sub_oid, sub_count) = recurse(store, provider, base_sub, sub_changes, policy)?;
        if sub_count == 0 {
            entries.remove(&name); // prune now-empty directory
        } else {
            entries.insert(
                name.clone(),
                TreeEntry {
                    name,
                    mode: GitMode::Tree,
                    object_id: sub_oid,
                },
            );
        }
    }

    // Apply direct changes.
    for (name, ch) in direct {
        match ch {
            TreeChange::Set { oid, mode } => {
                entries.insert(
                    name.clone(),
                    TreeEntry {
                        name,
                        mode,
                        object_id: oid,
                    },
                );
            }
            TreeChange::Remove => {
                entries.remove(&name);
            }
        }
    }

    let entries_vec: Vec<TreeEntry> = entries.into_values().collect();
    let count = entries_vec.len();
    let oid = store.write_tree(entries_vec)?;
    Ok((oid, count))
}
