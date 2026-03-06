use crate::ari_control::AriController;
use crate::ari_external_media::ExternalMediaRequest;
use crate::codec;
use crate::config::Config;
use crate::dial_string::DialString;
use crate::session::{Session, SessionState};
use crate::slmodem::socket_from_raw_fd;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Maximum bytes a single line from slmodemd can be before we discard it.
/// Dial strings are short (<128 chars); anything longer is garbled data.
const LINE_BUF_LIMIT: usize = 256;

/// Upper bound on consecutive calls in a single bridge lifetime. Prevents an
/// infinite loop if slmodemd keeps sending DIAL: commands without ever closing
/// the socket. 1000 calls is far beyond any realistic session.
const CALLS_LIMIT: u32 = 1000;

/// Maximum time to spend waiting for a DIAL: command before logging a
/// liveness heartbeat. Does not abort — just ensures we leave evidence in
/// logs that the bridge is alive and waiting. 60s is long enough to avoid
/// log spam, short enough to confirm the process isn't stuck.
const DIAL_WAIT_HEARTBEAT_SECS: u64 = 60;

/// 160 samples * 2 bytes (S16_LE) at 8000 Hz = one 20 ms frame of silence.
/// slmodemd's DSP expects a continuous clock of audio frames; starving it
/// causes internal buffer underruns that corrupt modem training sequences.
const SILENCE_FRAME_BYTES: usize = 320;

/// Interval between silence frames sent to slmodemd while idle. 20 ms matches
/// the codec frame duration so the DSP sees a steady sample clock.
const SILENCE_INTERVAL_MS: u64 = 20;

/// How often to log relay throughput stats. 2 seconds is frequent enough
/// to diagnose audio flow problems without flooding logs.
const RELAY_STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Safety limit on drain iterations during ARI setup. At 20 ms silence
/// intervals, 500 iterations = 10 seconds — well beyond any ARI setup
/// sequence. If we're still draining after this, something is stuck.
const DRAIN_ITERATIONS_LIMIT: u32 = 500;

#[allow(clippy::too_many_lines)]
pub async fn run(cfg: Config, media_request: ExternalMediaRequest) -> Result<()> {
    assert!(
        cfg.socket_fd >= 0,
        "socket fd must be non-negative, got {}",
        cfg.socket_fd
    );
    assert!(
        !cfg.ari_base_url.is_empty(),
        "ARI base URL must not be empty"
    );

    info!(
        fd = cfg.socket_fd,
        ari_base_url = %cfg.ari_base_url,
        "event=bridge_run_start"
    );

    let sl_stream = socket_from_raw_fd(cfg.socket_fd)?;
    let ari = AriController::from_config(&cfg)?;

    // Register the Stasis app by opening the ARI events WebSocket once.
    // Signal readiness to slmodemd only after this succeeds.
    info!("event=connecting_ari_events, reason=must register stasis app before signaling readiness");
    let ari_events = ari.connect_events(&media_request.app).await?;
    let ari_drain = tokio::spawn(drain_ari_events(ari_events));
    info!("event=ari_events_stream_established");

    // Signal to slmodemd that the audio path is ready.
    // 'R' = Ready. slmodemd blocks on socket_start() until it reads this byte.
    let mut sl_stream = sl_stream;
    sl_stream.writable().await?;
    sl_stream.write_all(b"R").await?;
    info!("event=sent_ready_to_slmodemd, reason=unblocks slmodemd modem emulation");

    let silence = [0u8; SILENCE_FRAME_BYTES];
    let mut calls_count: u32 = 0;

    loop {
        assert!(
            calls_count < CALLS_LIMIT,
            "exceeded call limit of {CALLS_LIMIT} — aborting to prevent runaway loop"
        );

        info!(calls_count, "event=waiting_for_dial_string");

        let (mut read_half, mut write_half) = sl_stream.into_split();
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(SILENCE_INTERVAL_MS));
        let mut line_buf = Vec::with_capacity(LINE_BUF_LIMIT);

        let dial_string = wait_for_dial_string(
            &mut read_half,
            &mut write_half,
            &mut interval,
            &mut line_buf,
            &silence,
        )
        .await;

        let dial_string = match dial_string {
            Some(ds) => ds,
            None => {
                info!("event=slmodemd_socket_closed, reason=normal shutdown, aborting ARI drain");
                ari_drain.abort();
                return Ok(());
            }
        };

        calls_count += 1;
        let dial_string = match DialString::parse(&dial_string) {
            Ok(parsed) => parsed.as_str().to_string(),
            Err(err) => {
                warn!(error = %err, raw = %dial_string, "event=invalid_dial_string");
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };
        info!(dial = %dial_string, calls_count, "event=received_dial_string");
        let mut session = Session::new(dial_string.clone());
        session.transition(SessionState::Originating)?;

        // ARI setup with concurrent slmodemd drain.
        //
        // Why: slmodemd starts its DSP immediately after sending DIAL:. It
        // writes modem tones to the socket and expects continuous received
        // audio frames. Without draining, the socket buffer fills with stale
        // audio that arrives as a burst when relay starts — corrupting modem
        // training. Without silence feed, the DSP underruns and its internal
        // timing drifts.
        let setup_result = {
            let setup_fut = ari_setup(
                &ari,
                &cfg,
                &media_request,
                &dial_string,
                &mut session,
            );
            drain_during_ari_setup(&mut read_half, &mut write_half, &silence, setup_fut).await
        };

        let (media_ws, call) = match setup_result {
            Ok(result) => result,
            Err(err) => {
                warn!(error = %err, dial = %dial_string, "event=ari_setup_failed, returning to wait state");
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };

        // Full bidirectional relay with slin↔ulaw codec conversion.
        session.transition(SessionState::MediaActive)?;
        info!(dial = %session.dial(), state = ?session.state(), "event=bridge_start");
        let started = tokio::time::Instant::now();

        let sl_stream_reunited = read_half
            .reunite(write_half)
            .map_err(|_| anyhow::anyhow!("failed to reunite socket halves for relay"))?;

        match relay_media(sl_stream_reunited, media_ws).await {
            Ok((sl_reunited, bytes_to_ws, bytes_to_sl)) => {
                let elapsed_ms = started.elapsed().as_millis();
                info!(
                    dial = %dial_string,
                    duration_ms = elapsed_ms,
                    tx_bytes = bytes_to_ws,
                    rx_bytes = bytes_to_sl,
                    "event=bridge_stop",
                );
                sl_stream = sl_reunited;
            }
            Err(e) => {
                error!(error = %e, dial = %dial_string, "event=media_relay_error");
                ari.teardown(&call).await;
                return Err(e);
            }
        }

        info!(dial = %dial_string, "event=teardown_start");
        ari.teardown(&call).await;
        info!(dial = %dial_string, "event=teardown_complete");
        session.transition(SessionState::Terminating)?;
    }
}

/// Perform all ARI setup: create media channel, connect WebSocket, originate
/// outbound call and bridge channels. Each step cleans up on failure.
async fn ari_setup(
    ari: &AriController,
    cfg: &Config,
    media_request: &ExternalMediaRequest,
    dial_string: &str,
    session: &mut Session,
) -> Result<(
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    crate::ari_control::ActiveCall,
)> {
    let media_setup = match ari.setup_media(media_request).await {
        Ok(setup) => {
            info!(
                bridge_id = %setup.bridge_id,
                media_channel_id = %setup.media_channel_id,
                "event=media_setup_complete"
            );
            setup
        }
        Err(err) => {
            warn!(error = %err, dial = dial_string, "event=media_setup_failed");
            return Err(err);
        }
    };
    session.transition(SessionState::ConnectingMedia)?;

    let media_ws = match ari
        .connect_media_websocket(&media_setup.media_websocket_url, cfg.connect_timeout)
        .await
    {
        Ok(ws) => {
            info!(
                url = %media_setup.media_websocket_url,
                "event=media_websocket_connected"
            );
            ws
        }
        Err(err) => {
            warn!(error = %err, "event=media_websocket_connect_failed");
            ari.cleanup_media(&media_setup).await;
            return Err(err);
        }
    };

    let call = match ari
        .dial_and_bridge(dial_string, &media_request.app, &media_setup)
        .await
    {
        Ok(call) => {
            info!(
                outbound_channel_id = %call.outbound_channel_id,
                bridge_id = %call.bridge_id,
                "event=dial_and_bridge_complete"
            );
            call
        }
        Err(err) => {
            warn!(error = %err, dial = dial_string, "event=dial_failed");
            ari.cleanup_media(&media_setup).await;
            return Err(err);
        }
    };

    Ok((media_ws, call))
}

/// Drain slmodemd audio and feed silence while an async ARI setup runs.
///
/// slmodemd starts its DSP immediately after sending DIAL:. It writes modem
/// tones to the socket and expects continuous received audio frames (20 ms
/// cadence). This function keeps the DSP alive during ARI HTTP calls by:
/// 1. Reading and discarding audio from slmodemd (prevents kernel buffer
///    overflow and stale-audio burst when relay starts)
/// 2. Writing silence frames every 20 ms (prevents DSP underrun and timing
///    drift that would corrupt modem training)
async fn drain_during_ari_setup<F, T>(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    silence: &[u8],
    setup_fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    assert!(
        silence.len() == SILENCE_FRAME_BYTES,
        "silence must be {SILENCE_FRAME_BYTES} bytes"
    );

    tokio::pin!(setup_fut);

    let mut drain_buf = [0u8; 2048];
    let mut silence_interval =
        tokio::time::interval(std::time::Duration::from_millis(SILENCE_INTERVAL_MS));
    let mut drained_bytes: u64 = 0;
    let mut iterations: u32 = 0;

    loop {
        assert!(
            iterations < DRAIN_ITERATIONS_LIMIT,
            "drain loop exceeded {DRAIN_ITERATIONS_LIMIT} iterations — ARI setup stuck"
        );
        iterations += 1;

        tokio::select! {
            result = &mut setup_fut => {
                if drained_bytes > 0 {
                    info!(
                        drained_bytes,
                        iterations,
                        "event=drain_during_setup_complete"
                    );
                }
                return result;
            }
            res = read_half.read(&mut drain_buf) => {
                match res {
                    Ok(0) => {
                        return Err(anyhow::anyhow!(
                            "slmodemd closed socket during ARI setup"
                        ));
                    }
                    Ok(n) => {
                        drained_bytes += n as u64;
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "slmodemd read error during ARI setup: {e}"
                        ));
                    }
                }
            }
            _ = silence_interval.tick() => {
                if let Err(e) = write_half.write_all(silence).await {
                    return Err(anyhow::anyhow!(
                        "failed to write silence during ARI setup: {e}"
                    ));
                }
            }
        }
    }
}

/// Read from slmodemd until we receive a "DIAL:<number>\n" line.
/// Returns `None` if slmodemd closes the socket.
async fn wait_for_dial_string(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    interval: &mut tokio::time::Interval,
    line_buf: &mut Vec<u8>,
    silence: &[u8],
) -> Option<String> {
    assert!(
        silence.len() == SILENCE_FRAME_BYTES,
        "silence buffer must be {SILENCE_FRAME_BYTES} bytes, got {}",
        silence.len()
    );

    let mut buf = [0u8; 1024];
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(
        DIAL_WAIT_HEARTBEAT_SECS,
    ));
    // Skip the first immediate tick so the first heartbeat fires after the full interval.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            res = read_half.read(&mut buf) => {
                let n = match res {
                    Ok(0) => {
                        info!("event=slmodemd_socket_eof, reason=slmodemd closed the control socket");
                        return None;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        warn!(error = %e, "event=slmodemd_read_error");
                        return None;
                    }
                };

                for &b in &buf[..n] {
                    if b == b'\n' {
                        let line = String::from_utf8_lossy(line_buf);
                        if let Some(ds) = line.strip_prefix("DIAL:") {
                            let ds = ds.trim().to_string();
                            if !ds.is_empty() {
                                return Some(ds);
                            }
                            debug!("event=empty_dial_string, reason=DIAL: prefix present but number is empty");
                        } else if !line.is_empty() {
                            debug!(line = %line, "event=unknown_line_from_slmodemd");
                        }
                        line_buf.clear();
                    } else {
                        line_buf.push(b);
                        if line_buf.len() > LINE_BUF_LIMIT {
                            warn!(
                                len = line_buf.len(),
                                limit = LINE_BUF_LIMIT,
                                "event=line_buffer_overflow, reason=discarding garbled data from slmodemd"
                            );
                            line_buf.clear();
                        }
                    }
                }
            }
            _ = interval.tick() => {
                if let Err(e) = write_half.write_all(silence).await {
                    warn!(error = %e, "event=silence_write_failed, reason=socket closing");
                    return None;
                }
            }
            _ = heartbeat.tick() => {
                info!("event=dial_wait_heartbeat, reason=still waiting for DIAL command from slmodemd");
            }
        }
    }
}

/// Bidirectional media relay between slmodemd unix socket and Asterisk media
/// WebSocket, with slin↔ulaw codec conversion.
///
/// slmodemd speaks signed 16-bit linear PCM (S16_LE, 8 kHz). Asterisk's
/// ExternalMedia WebSocket is configured for ulaw (G.711 µ-law). Converting
/// here lets Asterisk's bridge use proxy_media mode (true passthrough, no
/// transcoding) which gives the cleanest audio path for modem training.
///
/// Returns the reunited unix stream and byte counts for each direction.
async fn relay_media(
    sl_stream: tokio::net::UnixStream,
    media_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<(tokio::net::UnixStream, u64, u64)> {
    let (mut ws_writer, mut ws_reader) = media_ws.split();
    let (mut sl_reader, mut sl_writer) = sl_stream.into_split();

    info!("event=relay_media_start");

    // sl→ws: read slin from slmodemd, convert to ulaw, send to Asterisk.
    let sl_to_ws = async {
        let mut total: u64 = 0;
        let mut interval_bytes: u64 = 0;
        let mut frames: u64 = 0;
        let mut last_report = tokio::time::Instant::now();
        let mut buf = [0u8; 2048];
        // Residual byte from a previous read that split mid-sample. In
        // practice this never happens (slmodemd writes 320-byte frames),
        // but we handle it for correctness.
        let mut residual: Option<u8> = None;

        loop {
            let n = sl_reader
                .read(&mut buf)
                .await
                .context("failed reading from slmodemd socket")?;
            if n == 0 {
                debug!("event=sl_to_ws_eof, reason=slmodemd closed audio stream");
                let _ = ws_writer.send(Message::Close(None)).await;
                break;
            }

            let slin = codec::align_slin_buffer(&mut residual, &buf[..n]);
            if slin.is_empty() {
                continue;
            }

            let ulaw = codec::encode_ulaw(&slin);
            total += ulaw.len() as u64;
            interval_bytes += ulaw.len() as u64;
            frames += 1;

            ws_writer
                .send(Message::Binary(ulaw.into()))
                .await
                .context("failed writing modem audio to media websocket")?;

            if last_report.elapsed() >= RELAY_STATS_INTERVAL {
                let elapsed_ms = last_report.elapsed().as_millis();
                info!(
                    direction = "sl→ws",
                    interval_bytes,
                    interval_frames = frames,
                    interval_ms = elapsed_ms,
                    total_bytes = total,
                    "event=relay_throughput"
                );
                interval_bytes = 0;
                frames = 0;
                last_report = tokio::time::Instant::now();
            }
        }
        Result::<u64>::Ok(total)
    };

    // ws→sl: read ulaw from Asterisk, convert to slin, write to slmodemd.
    let ws_to_sl = async {
        let mut total: u64 = 0;
        let mut interval_bytes: u64 = 0;
        let mut frames: u64 = 0;
        let mut min_frame: usize = usize::MAX;
        let mut max_frame: usize = 0;
        let mut last_report = tokio::time::Instant::now();

        while let Some(frame) = ws_reader.next().await {
            let frame = frame.context("failed reading from media websocket")?;
            match frame {
                Message::Binary(data) => {
                    let len = data.len();
                    total += len as u64;
                    interval_bytes += len as u64;
                    frames += 1;
                    min_frame = min_frame.min(len);
                    max_frame = max_frame.max(len);

                    // Convert ulaw from Asterisk to slin for slmodemd's DSP.
                    let slin = codec::decode_ulaw(&data);
                    sl_writer
                        .write_all(&slin)
                        .await
                        .context("failed writing media audio to slmodemd socket")?;

                    if last_report.elapsed() >= RELAY_STATS_INTERVAL {
                        let elapsed_ms = last_report.elapsed().as_millis();
                        info!(
                            direction = "ws→sl",
                            interval_bytes,
                            interval_frames = frames,
                            interval_ms = elapsed_ms,
                            total_bytes = total,
                            min_frame_bytes = min_frame,
                            max_frame_bytes = max_frame,
                            "event=relay_throughput"
                        );
                        interval_bytes = 0;
                        frames = 0;
                        min_frame = usize::MAX;
                        max_frame = 0;
                        last_report = tokio::time::Instant::now();
                    }
                }
                Message::Close(reason) => {
                    debug!(?reason, "event=ws_to_sl_close, reason=asterisk closed media websocket");
                    break;
                }
                Message::Text(text) => {
                    // Asterisk's WebSocket channel driver sends a MEDIA_START
                    // text frame on connection with format, ptime, and
                    // optimal frame size. Log it for diagnostics.
                    if text.contains("MEDIA_START") {
                        info!(media_start = %text, "event=media_start_received");
                    } else {
                        warn!(text = %text, "event=unexpected_text_frame_on_media_ws");
                    }
                }
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {
                    debug!("event=unknown_ws_frame_type");
                }
            }
        }
        Result::<u64>::Ok(total)
    };

    let (bytes_to_ws, bytes_to_sl) =
        tokio::try_join!(sl_to_ws, ws_to_sl).context("media relay failed")?;

    info!(
        tx_bytes = bytes_to_ws,
        rx_bytes = bytes_to_sl,
        "event=relay_media_stop"
    );

    let sl_reunited = sl_reader
        .reunite(sl_writer)
        .map_err(|_| anyhow::anyhow!("failed to reunite sl_stream parts after relay"))?;

    Ok((sl_reunited, bytes_to_ws, bytes_to_sl))
}

/// Drain the ARI events WebSocket to keep the Stasis app registered.
/// This stream MUST stay open for the bridge's lifetime — dropping it
/// unregisters the app and Asterisk will reject subsequent ARI calls.
async fn drain_ari_events(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    info!("event=ari_event_drain_start");
    let (_writer, mut reader) = stream.split();

    /// Upper bound on ARI events to drain before logging a summary. Prevents
    /// unbounded silent consumption. 100k events at ~1/sec = ~28 hours; well
    /// beyond any single bridge session.
    const EVENTS_DRAIN_LOG_INTERVAL: u64 = 1000;
    const EVENTS_DRAIN_LIMIT: u64 = 100_000;

    let mut events_count: u64 = 0;

    while let Some(msg) = reader.next().await {
        events_count += 1;

        if events_count % EVENTS_DRAIN_LOG_INTERVAL == 0 {
            debug!(events_count, "event=ari_drain_heartbeat");
        }

        if events_count >= EVENTS_DRAIN_LIMIT {
            warn!(
                events_count,
                limit = EVENTS_DRAIN_LIMIT,
                "event=ari_drain_limit_reached, reason=exceeded event drain limit, stopping"
            );
            break;
        }

        match msg {
            Ok(Message::Close(reason)) => {
                info!(?reason, "event=ari_events_closed, reason=asterisk closed the events websocket");
                break;
            }
            Ok(Message::Text(text)) => {
                debug!(event_text = %text, "event=ari_event_received");
            }
            Err(err) => {
                error!(error = %err, "event=ari_events_error, reason=events websocket read failed");
                break;
            }
            _ => {}
        }
    }

    info!(events_count, "event=ari_event_drain_stop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_frame_size_matches_codec_frame() {
        // 160 samples * 2 bytes/sample (S16_LE) = 320 bytes per 20ms frame at 8000 Hz.
        assert_eq!(SILENCE_FRAME_BYTES, 320);
    }

    #[test]
    fn calls_limit_is_reasonable() {
        assert!(CALLS_LIMIT >= 100, "calls limit too low for realistic use");
        assert!(CALLS_LIMIT <= 10_000, "calls limit unreasonably high");
    }

    #[test]
    fn drain_iterations_limit_covers_setup_window() {
        // At 20ms per iteration, the limit should cover at least 5 seconds
        // of ARI setup time.
        let coverage_ms = DRAIN_ITERATIONS_LIMIT as u64 * SILENCE_INTERVAL_MS;
        assert!(
            coverage_ms >= 5000,
            "drain limit only covers {coverage_ms}ms, need at least 5000ms"
        );
    }
}
