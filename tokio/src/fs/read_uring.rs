use crate::fs::OpenOptions;
use crate::runtime::driver::op::Op;

use std::io;
use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::path::Path;

// this algorithm is inspired from rust std lib version 1.90.0
// https://doc.rust-lang.org/1.90.0/src/std/io/mod.rs.html#409
const PROBE_SIZE: usize = 32;
const PROBE_SIZE_U32: u32 = PROBE_SIZE as u32;

// Max bytes we can read using io uring submission at a time
// Currently set to block size of 64
// SAFETY: cannot be higher than u32::MAX for safe cast
const MAX_READ_SIZE: usize = 64 * 1024 * 1024;

pub(crate) async fn read_uring(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).open(path).await?;

    // TODO: use io uring in the future to obtain metadata
    let size_hint: Option<usize> = file.metadata().await.map(|m| m.len() as usize).ok();

    let fd: OwnedFd = file
        .try_into_std()
        .expect("unexpected in-flight operation detected")
        .into();

    // extra single capacity for the whole size to fit without any reallocation
    let buf = Vec::with_capacity(size_hint.unwrap_or(0));

    read_to_end_uring(size_hint, fd, buf).await
}

async fn read_to_end_uring(
    size_hint: Option<usize>,
    mut fd: OwnedFd,
    mut buf: Vec<u8>,
) -> io::Result<Vec<u8>> {
    let mut offset = 0;

    let start_cap = buf.capacity();

    // if buffer has no room and no size_hint, start with a small probe_read from 0 offset
    if (size_hint.is_none() || size_hint == Some(0)) && buf.capacity() - buf.len() < PROBE_SIZE {
        let (size_read, r_fd, r_buf) = small_probe_read(fd, buf, offset).await?;

        if size_read == 0 {
            return Ok(r_buf);
        }

        buf = r_buf;
        fd = r_fd;
        offset += size_read as u64;
    }

    loop {
        if buf.len() == buf.capacity() && buf.capacity() == start_cap {
            // The buffer might be an exact fit. Let's read into a probe buffer
            // and see if it returns `Ok(0)`. If so, we've avoided an
            // unnecessary increasing of the capacity. But if not, append the
            // probe buffer to the primary buffer and let its capacity grow.
            let (size_read, r_fd, r_buf) = small_probe_read(fd, buf, offset).await?;

            if size_read == 0 {
                return Ok(r_buf);
            }

            buf = r_buf;
            fd = r_fd;
            offset += size_read as u64;
        }

        // buf is full, need more capacity
        if buf.len() == buf.capacity() {
            buf.try_reserve(PROBE_SIZE)?;
        }

        // doesn't matter if we have a valid size_hint or not, if we do more
        // than 2 consecutive_short_reads, gradually increase the buffer
        // capacity to read more data at a time

        // prepare the spare capacity to be read into
        let buf_len = usize::min(buf.spare_capacity_mut().len(), MAX_READ_SIZE);

        // SAFETY: buf_len cannot be greater than u32::MAX because max_read_size
        // is u32::MAX
        let mut read_len = buf_len as u32;

        // read into spare capacity
        let res = op_read(fd, buf, read_len, offset).await;

        match res {
            Ok((Ok(0), _, r_buf)) => return Ok(r_buf),
            Ok((Ok(size_read), r_fd, r_buf)) => {
                fd = r_fd;
                buf = r_buf;
                offset += size_read as u64;
                read_len -= size_read;
            }
            Ok((Err(e), _, _)) | Err(e) => return Err(e),
        }
    }
}

async fn small_probe_read(
    mut fd: OwnedFd,
    mut buf: Vec<u8>,
    offset: u64,
) -> io::Result<(u32, OwnedFd, Vec<u8>)> {
    let mut temp_arr = [0; PROBE_SIZE];
    let has_enough = buf.len() > PROBE_SIZE;

    if has_enough {
        // if we have more than PROBE_SIZE bytes in the buffer already then
        // don't call reserve as we might potentially read 0 bytes
        let back_bytes_len = buf.len() - PROBE_SIZE;
        temp_arr.copy_from_slice(&buf[back_bytes_len..]);
        // We're decreasing the length of the buffer and len is greater
        // than PROBE_SIZE. So we can read into the discarded length
        buf.truncate(back_bytes_len);
    } else {
        // we don't even have PROBE_SIZE length in the buffer, we need this
        // reservation
        buf.reserve_exact(PROBE_SIZE);
    }

    let res = op_read(fd, buf, PROBE_SIZE_U32, offset).await;

    match res {
        // return early if we inserted into reserved PROBE_SIZE
        // bytes
        Ok((Ok(size_read), r_fd, r_buf)) if !has_enough => Ok((size_read, r_fd, r_buf)),
        Ok((Ok(size_read), r_fd, mut r_buf)) => {
            let old_len = r_buf.len() - (size_read as usize);

            r_buf.splice(old_len..old_len, temp_arr);

            Ok((size_read, r_fd, r_buf))
        }
        Ok((Err(e), _, _)) | Err(e) => Err(e),
    }
}

async fn op_read(
    mut fd: OwnedFd,
    mut buf: Vec<u8>,
    len: u32,
    offset: u64,
) -> io::Result<(io::Result<u32>, OwnedFd, Vec<u8>)> {
    loop {
        let (res, r_fd, r_buf) = Op::read(fd, buf, len, offset).await;

        match res {
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                buf = r_buf;
                fd = r_fd;
            }
            Err(e) => return Err(e),
            _ => return Ok((res, r_fd, r_buf)),
        }
    }
}
