use std::iter;
use std::path::Path;

use rand::{RngExt, SeedableRng, rngs::SmallRng};

#[derive(Clone, Copy, Debug)]
pub enum Workload {
    Head,
    Read,
    ReadThrice,
    Random,
}

impl Workload {
    pub const ALL: [Self; 4] = [Self::Head, Self::Read, Self::ReadThrice, Self::Random];

    pub fn comm(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Read | Self::ReadThrice => "sha256sum",
            Self::Random => "dd",
        }
    }

    pub fn script(self, file: &Path, mib: u64) -> String {
        let file = file.display();
        let mut rng = SmallRng::seed_from_u64(mib);
        let random_read = || {
            let skip = rng.random_range(0..mib);
            format!("dd if={file} of=/dev/null bs=1M count=1 skip={skip} status=none")
        };
        match self {
            Self::Head => format!("head -c 65536 {file} >/dev/null"),
            Self::Read => format!("sha256sum {file} >/dev/null"),
            Self::ReadThrice => format!("for i in 1 2 3; do sha256sum {file} >/dev/null; done"),
            Self::Random => iter::repeat_with(random_read)
                .take(20)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}
