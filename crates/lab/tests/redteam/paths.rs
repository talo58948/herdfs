//! Round 2: which entry a path names on a client after another client moves or replaces it.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::{VISIBLE_AFTER, assert_bytes, save, two_clients};

/// Repro: b appends to `f` and keeps it open; a renames `f` to `g` and saves a new `f`; after
/// T_vis b reads `f`. b's lookup answers from its cached entry while its journal holds changes for
/// that entry, so the path still names the old file, and b reads its own copy of what is now `g`.
// contract: C2
#[test]
fn a_pending_write_does_not_hide_a_new_file_at_its_old_path() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    save(&a.join("f"), b"old\n")?;
    thread::sleep(VISIBLE_AFTER);
    let mut writer = OpenOptions::new().append(true).open(b.join("f"))?;
    writer.write_all(b"b's line\n")?;

    fs::rename(a.join("f"), a.join("g"))?;
    save(&a.join("f"), b"new\n")?;
    thread::sleep(VISIBLE_AFTER);
    assert_bytes(
        &fs::read(b.join("f"))?,
        b"new\n",
        "b's read of f after a replaced it",
    );
    Ok(())
}

/// Repro: b reads `f`; a saves `f` the way editors do, a temporary file renamed over it; b reads
/// `f` again at once. b's kernel still holds `f`'s old inode from the lookup, the open asks the
/// authority for `f`, finds another entry there and answers ENOENT, though `f` never stopped
/// existing.
// contract: C7
#[test]
fn a_file_replaced_by_rename_never_vanishes_for_another_client() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    save(&a.join("f"), b"v0")?;
    let mut vanished = 0;
    for round in 1..=ROUNDS {
        thread::sleep(VISIBLE_AFTER);
        fs::read(b.join("f"))?;
        save(&a.join("f.tmp"), format!("v{round}").as_bytes())?;
        fs::rename(a.join("f.tmp"), a.join("f"))?;
        match fs::read(b.join("f")) {
            Err(error) if error.kind() == ErrorKind::NotFound => vanished += 1,
            read => drop(read?),
        }
    }
    assert_eq!(vanished, 0, "b found no f in {vanished} of {ROUNDS} rounds");
    Ok(())
}

const ROUNDS: usize = 5;

/// Repro: a shell on b changes into `d`; a renames `d` to `e` and makes a new, empty `d`; the
/// shell writes `note` in its working directory. b resolves names under the directory by the
/// path it last saw for it, so `note` lands in the new `d` instead of beside `e`'s other files.
// contract: C7
#[test]
fn a_file_made_in_a_moved_directory_lands_in_that_directory() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    fs::create_dir(a.join("d"))?;
    save(&a.join("d/f"), b"in d")?;
    thread::sleep(VISIBLE_AFTER);
    let shell = Command::new("sh")
        .arg("-c")
        .arg("cd \"$0\" && sleep 3 && echo made > note; cat f")
        .arg(b.join("d"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    thread::sleep(Duration::from_secs(1));
    fs::rename(a.join("d"), a.join("e"))?;
    fs::create_dir(a.join("d"))?;
    let shell = shell.wait_with_output()?;
    lab.wait_committed("b")?;
    thread::sleep(VISIBLE_AFTER);
    let stray = a.join("d/note").exists();
    assert!(!stray, "the note made in the moved directory is in the new d");
    let stderr = String::from_utf8_lossy(&shell.stderr);
    assert!(
        shell.status.success(),
        "the shell in the moved directory: {stderr}"
    );
    assert_bytes(&fs::read(a.join("e/note"))?, b"made\n", "e/note");
    Ok(())
}

/// Repro: b opens `f`; a renames `f` to `g` and saves a new `f` with mode 644; after T_vis b
/// changes the mode of its open file to 600. b sends the change for the path it last saw for the
/// file's inode, so it lands on the new `f`, a file b never opened, and `g` keeps its mode.
// contract: C7
#[test]
fn a_mode_change_through_an_open_file_follows_it_across_a_rename() -> Result<()> {
    let Some(lab) = two_clients()? else {
        return Ok(());
    };
    let (a, b) = (lab.mount_path("a"), lab.mount_path("b"));
    save(&a.join("f"), b"first")?;
    fs::set_permissions(a.join("f"), Permissions::from_mode(0o644))?;
    thread::sleep(VISIBLE_AFTER);
    let open = File::open(b.join("f"))?;

    fs::rename(a.join("f"), a.join("g"))?;
    save(&a.join("f"), b"second")?;
    fs::set_permissions(a.join("f"), Permissions::from_mode(0o644))?;
    thread::sleep(VISIBLE_AFTER);
    open.set_permissions(Permissions::from_mode(0o600))?;
    thread::sleep(VISIBLE_AFTER);
    let mode =
        |name: &str| -> Result<String> { Ok(format!("{:o}", fs::metadata(a.join(name))?.mode() & 0o777)) };
    assert_eq!(mode("f")?, "644", "the mode of the new f, which b never opened");
    assert_eq!(mode("g")?, "600", "the mode of g, the file b changed");
    Ok(())
}
