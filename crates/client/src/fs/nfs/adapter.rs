//! The `nfsserve` VFS implementation over one [`Space`](crate::fs::Space).

use async_trait::async_trait;
use nfsserve::nfs::{self, nfsstat3};
use nfsserve::vfs::{NFSFileSystem, ReadDirResult, VFSCapabilities, WriteStability};

use super::{MAX_FILE_SIZE, Nfs};

#[async_trait]
impl NFSFileSystem for Nfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> nfs::fileid3 {
        self.space.kernel.root().0
    }

    async fn lookup(&self, dirid: u64, filename: &nfs::filename3) -> Result<u64, nfsstat3> {
        self.lookup_entry(dirid, filename).await
    }

    async fn getattr(&self, id: u64) -> Result<nfs::fattr3, nfsstat3> {
        self.get_attr(id).await
    }

    async fn setattr(&self, id: u64, setattr: nfs::sattr3) -> Result<nfs::fattr3, nfsstat3> {
        self.set_attr(id, setattr).await
    }

    async fn read(&self, id: u64, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.read_epoch(herdfs_core::proto::EntryId(id), offset, count)
            .await
            .map_err(Self::stale)
    }

    async fn write(
        &self,
        id: u64,
        offset: u64,
        data: &[u8],
        stability: WriteStability,
    ) -> Result<(nfs::fattr3, WriteStability), nfsstat3> {
        let stable = stability != WriteStability::Unstable;
        self.write_generation(herdfs_core::proto::EntryId(id), offset, data, stable)
            .await
            .map_err(Self::stale)?;
        let attr = self.get_attr(id).await?;
        let committed = if stable {
            WriteStability::FileSync
        } else {
            WriteStability::Unstable
        };
        Ok((attr, committed))
    }

    async fn commit(&self, id: u64) -> Result<nfs::fattr3, nfsstat3> {
        self.commit_generation(herdfs_core::proto::EntryId(id))
            .await
            .map_err(Self::stale)?;
        self.get_attr(id).await
    }

    async fn create(
        &self,
        dirid: u64,
        filename: &nfs::filename3,
        attr: nfs::sattr3,
        guarded: bool,
    ) -> Result<(u64, nfs::fattr3), nfsstat3> {
        self.create_file(dirid, filename, attr, guarded).await
    }

    async fn create_exclusive(
        &self,
        dirid: u64,
        filename: &nfs::filename3,
        verifier: nfs::createverf3,
    ) -> Result<u64, nfsstat3> {
        self.create_exclusive_file(dirid, filename, verifier).await
    }

    async fn mkdir(
        &self,
        dirid: u64,
        dirname: &nfs::filename3,
        attr: nfs::sattr3,
    ) -> Result<(u64, nfs::fattr3), nfsstat3> {
        self.make_dir(dirid, dirname, attr).await
    }

    async fn remove(&self, dirid: u64, filename: &nfs::filename3) -> Result<(), nfsstat3> {
        self.remove_name(dirid, filename, false).await
    }

    async fn rmdir(&self, dirid: u64, dirname: &nfs::filename3) -> Result<(), nfsstat3> {
        self.remove_name(dirid, dirname, true).await
    }

    async fn rename(
        &self,
        from_dirid: u64,
        from_filename: &nfs::filename3,
        to_dirid: u64,
        to_filename: &nfs::filename3,
    ) -> Result<(), nfsstat3> {
        self.space
            .clone()
            .rename(
                (self.ino(from_dirid), Self::name(from_filename)?),
                (self.ino(to_dirid), Self::name(to_filename)?),
            )
            .await
            .map(drop)?;
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: u64,
        start_after: u64,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        self.list_dir(dirid, start_after, max_entries).await
    }

    async fn symlink(
        &self,
        dirid: u64,
        linkname: &nfs::filename3,
        symlink: &nfs::nfspath3,
        attr: &nfs::sattr3,
    ) -> Result<(u64, nfs::fattr3), nfsstat3> {
        self.make_symlink(dirid, linkname, symlink, *attr).await
    }

    async fn readlink(&self, id: u64) -> Result<nfs::nfspath3, nfsstat3> {
        self.space
            .clone()
            .readlink(self.ino(id))
            .await
            .map(Into::into)
            .map_err(Self::stale)
    }

    async fn fsinfo(&self, root_fileid: u64) -> Result<nfs::fsinfo3, nfsstat3> {
        let attr = self.get_attr(root_fileid).await?;
        Ok(nfs::fsinfo3 {
            obj_attributes: nfs::post_op_attr::attributes(attr),
            rtmax: 131_072,
            rtpref: 131_072,
            rtmult: 4096,
            wtmax: 131_072,
            wtpref: 131_072,
            wtmult: 4096,
            dtpref: 131_072,
            maxfilesize: MAX_FILE_SIZE,
            time_delta: nfs::nfstime3 {
                seconds: 0,
                nseconds: 1_000_000,
            },
            properties: nfs::FSF_SYMLINK | nfs::FSF_HOMOGENEOUS | nfs::FSF_CANSETTIME,
        })
    }

    fn id_to_fh(&self, id: u64) -> nfs::nfs_fh3 {
        let mut data = Vec::with_capacity(32);
        data.extend_from_slice(&self.handle_prefix);
        data.extend_from_slice(&id.to_be_bytes());
        nfs::nfs_fh3 { data }
    }

    fn fh_to_id(&self, handle: &nfs::nfs_fh3) -> Result<u64, nfsstat3> {
        if handle.data.len() != 32 || handle.data[..24] != self.handle_prefix {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        let mut encoded = [0; 8];
        encoded.copy_from_slice(&handle.data[24..]);
        let id = u64::from_be_bytes(encoded);
        if id == 0 || id > i64::MAX as u64 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        Ok(id)
    }

    fn write_verifier(&self) -> nfs::writeverf3 {
        self.verifier
    }
}
