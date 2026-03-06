use anyhow::{Context, Result};
use std::os::fd::FromRawFd;
use tracing::info;

/// Maximum fd value we accept. Linux's default limit is 1024 for unprivileged
/// processes; slmodemd typically passes fd 3 or 4. Anything above 4096 is
/// almost certainly a bug (garbled args, wrong fd inheritance).
const FD_LIMIT: i32 = 4096;

pub fn socket_from_raw_fd(fd: i32) -> Result<tokio::net::UnixStream> {
    assert!(fd >= 0, "socket fd must be non-negative, got {fd}");
    assert!(fd < FD_LIMIT, "socket fd {fd} exceeds limit {FD_LIMIT} — likely garbled args from slmodemd");

    info!(fd, "event=converting_raw_fd_to_unix_stream");

    let std_stream = unsafe {
        // slmodemd passes an owned fd for this child process.
        std::os::unix::net::UnixStream::from_raw_fd(fd)
    };

    std_stream
        .set_nonblocking(true)
        .context("failed to set slmodemd socket to nonblocking")?;

    let stream = tokio::net::UnixStream::from_std(std_stream)
        .context("failed to convert slmodemd fd to tokio stream")?;

    info!(fd, "event=unix_stream_ready");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::socket_from_raw_fd;

    #[test]
    fn rejects_negative_fd() {
        let result = std::panic::catch_unwind(|| socket_from_raw_fd(-1));
        assert!(result.is_err(), "negative fd must panic via assertion");
    }
}
