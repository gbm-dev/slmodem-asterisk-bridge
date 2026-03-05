use anyhow::{Context, Result};
use std::os::fd::FromRawFd;

pub fn socket_from_raw_fd(fd: i32) -> Result<tokio::net::UnixStream> {
    if fd < 0 {
        anyhow::bail!("invalid socket fd: {fd}");
    }

    let std_stream = unsafe {
        // slmodemd passes an owned fd for this child process.
        std::os::unix::net::UnixStream::from_raw_fd(fd)
    };

    std_stream
        .set_nonblocking(true)
        .context("failed to set slmodemd socket to nonblocking")?;

    tokio::net::UnixStream::from_std(std_stream)
        .context("failed to convert slmodemd fd to tokio stream")
}

#[cfg(test)]
mod tests {
    use super::socket_from_raw_fd;

    #[test]
    fn rejects_negative_fd() {
        let err = socket_from_raw_fd(-1).expect_err("negative fd must fail");
        assert!(err.to_string().contains("invalid socket fd"));
    }
}
