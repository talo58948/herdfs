//! Red team: confirmed contract breaks, each reduced to one failing test. Rounds 1 to 7 are fixed
//! and run; round 7's are ignored until fixed. Skips until the binaries are built.

mod agents;
mod appends;
mod atomic_saves;
mod build_mtimes;
mod collection_reads;
mod commit_manifest;
mod crashed_rewrites;
mod crashed_saves;
mod creates;
mod crowded_writes;
mod debounce;
mod followed;
mod followed_paths;
mod forgotten_replays;
mod helpers;
mod journal;
mod listed_heads;
mod listed_renames;
mod lock_files;
mod locks;
mod mapped;
mod moved_removes;
mod mtime;
mod offline;
mod offline_heads;
mod offline_reads;
mod offline_renames;
mod offline_writes;
mod open_handles;
mod paths;
mod pending_opens;
mod pin_switches;
mod raced_renames;
mod recovery;
mod refused_fsync;
mod rejected_writes;
mod remapped;
mod replay_answers;
mod replayed_commits;
mod replayed_heads;
mod revoked_replays;
mod rewound_reads;
mod round21_rename_race;
mod round22_agents;
mod round23_local_pin;
mod round23_rename_settle;
mod round23_reused_parent;
mod round25_moving_parent;
mod round27_parent_ops;
mod round29_empty_pin;
mod round29_rename_source;
mod round30_agents_acl;
mod round30_agents_race;
mod round31_offline_held;
mod round31_pending_pin;
mod round32_agents_cwd;
mod round33_relocated_child;
mod round35_held_entries;
mod round37_sequences;
mod round37_swapped_dirs;
mod round39_client;
mod round43_client;
mod round45_client;
mod round47_client;
mod round49_client;
mod round51_cache;
mod round51_journal;
mod round53_client;
mod round55_client;
mod round57_kept;
mod round57_plans;
mod round59_control;
mod round59_nfs;
mod round59_plans;
mod round61_kept;
mod round61_nfs;
mod round63_kept;
mod round63_nfs;
mod round63_plans;
mod round65_kept;
mod round65_nfs;
mod round65_plans;
mod round67_kept;
mod round67_plans;
mod saved_mtime;
mod scratch_files;
mod shared_appends;
mod shared_builds;
mod slow_links;
mod slow_uploads;
mod stalled_reads;
mod stalled_stores;
mod synced_opens;
mod times;
mod unstored_uploads;
mod unusable_versions;
mod versions;

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::time::Duration;

use rustix::fs::{AtFlags, Dir, Mode, OFlags};

use crate::helpers::*;

#[allow(dead_code, unused_imports)]
#[path = "../nfs/support.rs"]
mod nfs_support;

/// T_vis plus a margin for the test's own scheduling.
const VISIBLE_AFTER: Duration = Duration::from_millis(1_200);
/// Opens `name` relative to the held directory, without recovering a path for it.
fn open_entry(dir: &File, name: &str, flags: OFlags) -> io::Result<File> {
    Ok(File::from(rustix::fs::openat(
        dir,
        name,
        flags | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?))
}

fn read_held(file: &File) -> io::Result<Vec<u8>> {
    read_at(
        file,
        0,
        usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?,
    )
}

fn read_entry(dir: &File, name: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_entry(dir, name, OFlags::RDONLY)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn write_entry(dir: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    open_entry(dir, name, OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC)?.write_all(bytes)
}

fn save_entry(dir: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    let mut file = open_entry(dir, name, OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn create_entry(dir: &File, name: &str) -> io::Result<File> {
    open_entry(dir, name, OFlags::WRONLY | OFlags::CREATE)
}

fn mkdir_entry(dir: &File, name: &str) -> io::Result<()> {
    Ok(rustix::fs::mkdirat(
        dir,
        name,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )?)
}

fn unlink_entry(dir: &File, name: &str) -> io::Result<()> {
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?)
}

fn rmdir_entry(dir: &File, name: &str) -> io::Result<()> {
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

fn rename_entry(from: &File, name: &str, to: &File, new_name: &str) -> io::Result<()> {
    Ok(rustix::fs::renameat(from, name, to, new_name)?)
}

fn symlink_entry(dir: &File, name: &str, target: &str) -> io::Result<()> {
    Ok(rustix::fs::symlinkat(target, dir, name)?)
}

fn readlink_entry(dir: &File, name: &str) -> io::Result<OsString> {
    let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
    Ok(OsString::from_vec(target.into_bytes()))
}

fn entry_names(dir: &File) -> io::Result<Vec<OsString>> {
    let fd = rustix::io::dup(dir)?;
    Dir::read_from(fd)?
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_name().to_bytes() == b"." || entry.file_name().to_bytes() == b".." => {
                None
            }
            Ok(entry) => Some(Ok(OsString::from_vec(entry.file_name().to_bytes().to_vec()))),
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}
