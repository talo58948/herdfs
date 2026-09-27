//! The workload's random choices: a fixed generator, and who, where and what to pick.

use herdfs_core::{
    ChunkId, ChunkRef, SpacePath,
    proto::{Kind, VersionId},
};

use crate::{NAMES, Sim};

/// SplitMix64: small, and fixed forever, so recorded seeds keep replaying.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())].clone())
    }
}

impl Sim {
    pub fn actor(&mut self) -> usize {
        [0, 0, 0, 0, 0, 1, 1, 1, 2, 2][self.rng.below(10)]
    }

    pub fn space(&mut self) -> String {
        let spaces: Vec<_> = self.model.spaces.keys().cloned().collect();
        match self.rng.chance(95) {
            true => self.rng.pick(&spaces).expect("space s always exists"),
            false => "nowhere".into(),
        }
    }

    /// A path to create or move to: mostly a new name in a directory of `space`.
    pub fn path(&mut self, space: &str) -> SpacePath {
        let name = NAMES[self.rng.below(NAMES.len())];
        let dirs: Vec<_> = self.model.spaces.get(space).map_or_else(Vec::new, |known| {
            let shallow = |path: &SpacePath| path.as_str().matches('/').count() < 2;
            let dirs = known
                .nodes
                .iter()
                .filter(|(path, node)| node.kind == Kind::Dir && shallow(path));
            dirs.map(|(path, _)| path.clone()).collect()
        });
        if self.rng.chance(60)
            && let Some(dir) = self.rng.pick(&dirs)
        {
            return dir.join(name).unwrap();
        }
        let depth = if self.rng.chance(6) {
            0
        } else {
            1 + self.rng.below(3)
        };
        let names: Vec<_> = (0..depth).map(|_| NAMES[self.rng.below(NAMES.len())]).collect();
        SpacePath::try_from(names.join("/")).unwrap()
    }

    /// Mostly a path that exists in `space`, and rarely its root.
    pub fn existing_path(&mut self, space: &str) -> SpacePath {
        if self.rng.chance(4) {
            return SpacePath::root();
        }
        let paths: Vec<_> = self.model.spaces.get(space).map_or_else(Vec::new, |known| {
            known
                .nodes
                .keys()
                .filter(|path| !path.is_root())
                .cloned()
                .collect()
        });
        match self.rng.chance(80) {
            true => self.rng.pick(&paths).unwrap_or_else(|| self.path(space)),
            false => self.path(space),
        }
    }

    pub fn chunk(&mut self) -> ChunkRef {
        let id = ChunkId::of(format!("chunk {}", self.chunks.len()).as_bytes());
        let chunk = ChunkRef {
            id,
            len: 1 + self.rng.below(1 << 20) as u32,
        };
        self.chunks.push(chunk);
        chunk
    }

    pub fn versions(&self) -> Vec<VersionId> {
        self.model.versions.keys().copied().collect()
    }
}
