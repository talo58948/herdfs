//! Round 13: the reads of one open file on a client after the version it follows becomes unusable
//! to that client and a read moves it on to the head.

use std::fs::{self, File};
use std::thread;

use anyhow::Result;
use herdfs_core::SpacePath;
use herdfs_core::proto::{Perm, Request};
use herdfs_lab::{ADMIN, Lab, SPACE};

use crate::{VISIBLE_AFTER, random_bytes, read_at, save, two_clients};

/// Several chunks, so a read of the first page leaves the last one unfetched.
const LEN: usize = 12 << 20;
const PAGE: usize = 4096;
const LAST: usize = LEN - PAGE;
/// Where b's first reads start: the kernel caches v1 from there to the end.
const HALF: usize = LEN / 2;
const TRIALS: usize = 5;

/// Grants bob read on `shared` anew, so that versions committed before now are hidden from him
/// unless they are the head (S8).
fn regrant(lab: &Lab) -> Result<()> {
    for perm in [None, Some(Perm::Read)] {
        let request = Request::SetAcl {
            space: SPACE.into(),
            path: SpacePath::try_from("shared")?,
            principal: "user:bob".into(),
            perm,
        };
        lab.rpc(ADMIN, &request)?;
    }
    Ok(())
}

/// Bob's only access is read on `shared`. In each of 5 trials, a saves v1 of a new file there;
/// with `hidden`, bob is granted read on `shared` anew. b opens the file and reads its second half;
/// a saves v2. After T_vis, without `hidden`, b stats the file, which moves it on to v2; b reads
/// the first page through the same descriptor, which v2 answers, then the last page again.
/// Returns in how many trials that second read of the last page gave v1's bytes.
fn rewound_reads(hidden: bool) -> Result<Option<usize>> {
    let Some(lab) = two_clients()? else {
        return Ok(None);
    };
    fs::create_dir(lab.mount_path("a").join("shared"))?;
    regrant(&lab)?;
    let root = Request::SetAcl {
        space: SPACE.into(),
        path: SpacePath::root(),
        principal: "user:bob".into(),
        perm: None,
    };
    lab.rpc(ADMIN, &root)?;
    let mut rewound = 0;
    for trial in 0..TRIALS {
        let name = format!("shared/f{trial}");
        let (v1, v2) = (random_bytes(LEN), random_bytes(LEN));
        save(&lab.mount_path("a").join(&name), &v1)?;
        if hidden {
            regrant(&lab)?;
        }
        thread::sleep(VISIBLE_AFTER);
        let file = File::open(lab.mount_path("b").join(&name))?;
        read_at(&file, HALF as u64, LEN - HALF)?;

        save(&lab.mount_path("a").join(&name), &v2)?;
        thread::sleep(VISIBLE_AFTER);
        if !hidden {
            fs::metadata(lab.mount_path("b").join(&name))?;
        }
        let first = read_at(&file, 0, PAGE)?;
        let last = read_at(&file, LAST as u64, PAGE)?;
        anyhow::ensure!(first == v2[..PAGE], "trial {trial}: the first page reads v2");
        rewound += usize::from(last == v1[LAST..]);
    }
    Ok(Some(rewound))
}

/// Repro: [`rewound_reads`] with the grant renewed after each v1, so v1 is hidden from bob once v2
/// is the head (S8). The read of the first page finds v1 unusable and moves the file on to v2
/// (fs/ask.rs `usable`), asking the kernel to drop the file's cached pages without waiting
/// (`Kernel::apply` with `wait` false), and answers from v2; the next read of the last page comes
/// back before the kernel has dropped v1's copy of it and returns v1's bytes. The file goes back
/// to v1 after a read returned v2, which a local file another program rewrote never does. The
/// control is [`an_open_file_never_reads_an_older_version_after_a_stat`], where bob's grant
/// predates v1 and a stat moves the file on to v2, which waits until the kernel has dropped v1's
/// pages.
// contract: C3 (an open file follows its latest version between reads, as a local file does)
#[test]
fn an_open_file_never_reads_an_older_version_after_its_version_is_hidden() -> Result<()> {
    let Some(rewound) = rewound_reads(true)? else {
        return Ok(());
    };
    assert_eq!(
        rewound, 0,
        "trials whose last page read v1 after the first read v2, of {TRIALS}"
    );
    Ok(())
}

// contract: C3
#[test]
fn an_open_file_never_reads_an_older_version_after_a_stat() -> Result<()> {
    let Some(rewound) = rewound_reads(false)? else {
        return Ok(());
    };
    assert_eq!(
        rewound, 0,
        "trials whose last page read v1 after the first read v2, of {TRIALS}"
    );
    Ok(())
}
