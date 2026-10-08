use anyhow::{Context, Result, ensure};

pub fn denied_auth(reply: &[u8]) -> Result<bool> {
    Ok(word(reply, 4)? == 1 && word(reply, 8)? == 1 && word(reply, 12)? == 1)
}

fn word(bytes: &[u8], at: usize) -> Result<u32> {
    let encoded = bytes.get(at..at + 4).context("short RPC reply")?;
    Ok(u32::from_be_bytes(encoded.try_into()?))
}

pub struct Reply<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reply<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self> {
        ensure!(word(bytes, 4)? == 1, "RPC did not reply");
        ensure!(word(bytes, 8)? == 0, "RPC call was denied");
        ensure!(word(bytes, 20)? == 0, "RPC call was not accepted");
        Ok(Self { bytes, at: 24 })
    }

    pub fn u32(&mut self) -> Result<u32> {
        let value = word(self.bytes, self.at)?;
        self.at += 4;
        Ok(value)
    }

    pub fn u64(&mut self) -> Result<u64> {
        let encoded = self.bytes.get(self.at..self.at + 8).context("short XDR u64")?;
        self.at += 8;
        Ok(u64::from_be_bytes(encoded.try_into()?))
    }

    pub fn opaque(&mut self) -> Result<Vec<u8>> {
        let len = self.u32()? as usize;
        let value = self
            .bytes
            .get(self.at..self.at + len)
            .context("short XDR opaque")?;
        self.at += len.next_multiple_of(4);
        Ok(value.to_vec())
    }

    pub fn skip(&mut self, len: usize) -> Result<()> {
        ensure!(self.at + len <= self.bytes.len(), "short XDR value");
        self.at += len;
        Ok(())
    }

    pub fn fattr_size(&mut self) -> Result<u64> {
        self.skip(5 * 4)?;
        self.u64()
    }

    pub fn post_attr(&mut self) -> Result<()> {
        if self.u32()? != 0 {
            self.skip(84)?;
        }
        Ok(())
    }

    pub fn wcc(&mut self) -> Result<()> {
        if self.u32()? != 0 {
            self.skip(24)?;
        }
        self.post_attr()
    }
}
