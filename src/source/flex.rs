//! FlexRadio SmartSDR (6000/8000 series) DAX IQ client.
//!
//! Control: TCP port 4992, line protocol `C<seq>|<command>` / `R<seq>|<err>|<msg>` / `S<handle>|<status>`.
//! Data:    VITA-49 UDP packets sent to the port registered with `client udpport`.
//! The DAX IQ channel must be assigned to a panadapter (in SmartSDR, or any other client);
//! the stream's center frequency follows that panadapter.

use super::{SliceState, SourceEvent};
use super::vita;
use crossbeam_channel::{Receiver, Sender, unbounded};
use rustfft::num_complex::Complex32;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const CONTROL_PORT: u16 = 4992;

/// Requests from the GUI to the radio.
pub enum RadioCmd {
    /// Tune the slice with this letter (A-D) to a frequency in Hz, mode CW.
    TuneCw { slice: char, hz: f64 },
    /// Change the DAX IQ sample rate (24000, 48000, 96000 or 192000).
    SetRate(u32),
}

#[derive(Default)]
struct Slice {
    letter: String,
    mhz: Option<f64>,
    mode: String,
    in_use: bool,
    pan: String,
}

/// Open slices on panadapter `pan` (all of them while it isn't known yet), sorted by letter.
fn slices_on(slices: &HashMap<String, Slice>, pan: Option<&str>) -> Vec<SliceState> {
    let mut list: Vec<SliceState> = slices
        .values()
        .filter(|s| s.in_use && pan.is_none_or(|p| s.pan == p))
        .filter_map(|s| {
            let letter = s.letter.chars().next()?;
            s.mhz.map(|f| SliceState { letter, hz: f * 1e6, mode: s.mode.clone() })
        })
        .collect();
    list.sort_by_key(|s| s.letter);
    list
}
pub const DISCOVERY_PORT: u16 = 4992;

#[derive(Clone, Debug, Default)]
pub struct RadioInfo {
    pub ip: String,
    pub model: String,
    pub serial: String,
    pub nickname: String,
    pub callsign: String,
}

impl RadioInfo {
    pub fn describe(&self) -> String {
        format!("{} {} ({}) {}", self.model, self.nickname, self.callsign, self.ip)
    }
}

/// Parse space-separated `key=value` pairs.
fn kv(s: &str) -> HashMap<&str, &str> {
    s.split_whitespace().filter_map(|t| t.split_once('=')).collect()
}

/// Listen for SmartSDR discovery broadcasts for up to `timeout`.
pub fn discover(timeout: Duration) -> Result<Vec<RadioInfo>, String> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    sock.set_reuse_address(true).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    sock.set_reuse_port(true).map_err(|e| e.to_string())?;
    sock.bind(&SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT)).into())
        .map_err(|e| format!("bind UDP {DISCOVERY_PORT}: {e}"))?;
    let udp: UdpSocket = sock.into();
    udp.set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<RadioInfo> = Vec::new();
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let Ok((n, from)) = udp.recv_from(&mut buf) else { continue };
        let Some(p) = vita::parse(&buf[..n]) else { continue };
        if p.packet_class != vita::CLASS_DISCOVERY {
            continue;
        }
        let text = String::from_utf8_lossy(p.payload);
        let text = text.trim_end_matches('\0');
        let m = kv(text);
        let info = RadioInfo {
            ip: m.get("ip").map(|s| s.to_string()).unwrap_or_else(|| from.ip().to_string()),
            model: m.get("model").unwrap_or(&"").to_string(),
            serial: m.get("serial").unwrap_or(&"").to_string(),
            nickname: m.get("nickname").unwrap_or(&"").replace('\u{7f}', " "),
            callsign: m.get("callsign").unwrap_or(&"").to_string(),
        };
        if !found.iter().any(|r| r.serial == info.serial && r.ip == info.ip) {
            found.push(info);
        }
    }
    Ok(found)
}

struct Control {
    stream: TcpStream,
    seq: u32,
    debug: bool,
}

impl Control {
    fn send(&mut self, cmd: &str) -> u32 {
        self.seq += 1;
        if self.debug {
            eprintln!(">> C{}|{}", self.seq, cmd);
        }
        let _ = write!(self.stream, "C{}|{}\n", self.seq, cmd);
        self.seq
    }
}

pub fn start(
    addr: &str,
    channel: u8,
    want_rate: Option<u32>,
    tx: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
) -> Result<(Vec<JoinHandle<()>>, Sender<RadioCmd>), String> {
    let target = if addr.contains(':') { addr.to_string() } else { format!("{addr}:{CONTROL_PORT}") };
    let sa = target
        .to_socket_addrs()
        .map_err(|e| format!("{target}: {e}"))?
        .next()
        .ok_or_else(|| format!("{target}: no address"))?;
    let stream = TcpStream::connect_timeout(&sa, Duration::from_secs(3)).map_err(|e| format!("connect {sa}: {e}"))?;
    stream.set_nodelay(true).ok();
    // Short timeout so GUI commands (tuning) are picked up promptly between status lines.
    stream.set_read_timeout(Some(Duration::from_millis(50))).map_err(|e| e.to_string())?;
    let udp = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    udp.set_read_timeout(Some(Duration::from_millis(300))).map_err(|e| e.to_string())?;
    let _ = udp.set_recv_buffer_size_hint();
    let udp_port = udp.local_addr().map_err(|e| e.to_string())?.port();
    let our_stream = Arc::new(StreamInfo::default());
    let (cmd_tx, cmd_rx) = unbounded();

    let ctl_thread = {
        let tx = tx.clone();
        let stop = stop.clone();
        let our_stream = our_stream.clone();
        let udp = udp.try_clone().map_err(|e| e.to_string())?;
        let radio_ip = sa.ip();
        std::thread::Builder::new()
            .name("flex-control".into())
            .spawn(move || {
                control_loop(stream, udp, radio_ip, udp_port, channel, want_rate, tx, stop, our_stream, cmd_rx)
            })
            .map_err(|e| e.to_string())?
    };
    let data_thread = std::thread::Builder::new()
        .name("flex-vita".into())
        .spawn(move || data_loop(udp, tx, stop, our_stream))
        .map_err(|e| e.to_string())?;
    Ok((vec![ctl_thread, data_thread], cmd_tx))
}

trait RecvBufHint {
    fn set_recv_buffer_size_hint(&self) -> std::io::Result<()>;
}

impl RecvBufHint for UdpSocket {
    /// Ask for a large socket receive buffer: 192 kHz DAX IQ is ~12 Mbit/s.
    fn set_recv_buffer_size_hint(&self) -> std::io::Result<()> {
        socket2::SockRef::from(self).set_recv_buffer_size(4 << 20)
    }
}

#[allow(clippy::too_many_arguments)]
fn control_loop(
    stream: TcpStream,
    udp: UdpSocket,
    radio_ip: std::net::IpAddr,
    udp_port: u16,
    channel: u8,
    mut want_rate: Option<u32>,
    tx: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
    our_stream: Arc<StreamInfo>,
    cmd_rx: Receiver<RadioCmd>,
) {
    let debug = std::env::var_os("IQDBG").is_some();
    let mut reader = BufReader::new(stream.try_clone().expect("clone tcp"));
    let mut ctl = Control { stream, seq: 0, debug };
    let status = |s: String| {
        let _ = tx.send(SourceEvent::Status(s));
    };
    status(format!("Connected to {radio_ip}"));

    ctl.send("client program IQSkimmer");
    ctl.send(&format!("client udpport {udp_port}"));
    ctl.send("sub pan all");
    ctl.send("sub daxiq all");
    ctl.send("sub stream all");
    ctl.send("sub slice all");
    let mut create_seq = ctl.send(&format!("stream create type=dax_iq daxiq_channel={channel}"));
    let mut legacy_tried = false;

    // panadapter id -> (center MHz, daxiq channel)
    let mut pans: HashMap<String, (Option<f64>, Option<u8>)> = HashMap::new();
    let mut our_pan: Option<String> = None;
    let mut last_center: Option<f64> = None;
    let ch = channel.to_string();
    let mut slices: HashMap<String, Slice> = HashMap::new();
    let mut last_slices: Vec<SliceState> = Vec::new();
    // seq of the last `slice tune`, with the slice id and frequency
    let mut tune_seq: Option<(u32, String, f64)> = None;

    let mut line = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RadioCmd::SetRate(r) => {
                    // Before the stream exists, the rate is applied when it is created.
                    want_rate = Some(r);
                    let id = our_stream.id.load(Ordering::Relaxed);
                    if id != 0 {
                        ctl.send(&format!("stream set 0x{id:08X} daxiq_rate={r}"));
                    }
                }
                RadioCmd::TuneCw { slice, hz } => {
                    let letter = slice.to_string();
                    match slices.iter().find(|(_, s)| s.in_use && s.letter == letter) {
                        Some((id, sl)) => {
                            if sl.mode != "CW" {
                                ctl.send(&format!("slice set {id} mode=CW"));
                            }
                            let seq = ctl.send(&format!("slice tune {id} {:.6}", hz / 1e6));
                            tune_seq = Some((seq, id.clone(), hz));
                        }
                        None => status(format!("Slice {slice} isn't open on the radio")),
                    }
                }
            }
        }
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => {
                status("Radio closed the connection".into());
                break;
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => {
                status(format!("Control connection error: {e}"));
                break;
            }
        }
        let text = String::from_utf8_lossy(&line).trim().to_string();
        line.clear();
        if debug {
            eprintln!("<< {text}");
        }
        let Some(kind) = text.chars().next() else { continue };
        let body = &text[1..];
        match kind {
            'H' => {
                // Newer firmware also accepts UDP registration from the client's data socket.
                let msg = format!("client udp_register handle=0x{body}");
                let _ = udp.send_to(msg.as_bytes(), (radio_ip, 4993));
            }
            'R' => {
                let mut parts = body.splitn(3, '|');
                let seq: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let code = u32::from_str_radix(parts.next().unwrap_or("1"), 16).unwrap_or(1);
                let msg = parts.next().unwrap_or("");
                if let Some((tseq, id, hz)) = &tune_seq {
                    if seq == *tseq {
                        if code == 0 {
                            // The radio doesn't echo slice status back to the client that tuned it.
                            if let Some(sl) = slices.get_mut(id) {
                                sl.mhz = Some(hz / 1e6);
                                sl.mode = "CW".into();
                                status(format!("Slice {} tuned to {:.2} kHz CW", sl.letter, hz / 1e3));
                            }
                        } else {
                            status(format!("Tune failed: error 0x{code:08X} {msg}"));
                        }
                        tune_seq = None;
                    }
                }
                if seq == create_seq {
                    if code == 0 {
                        let id = u32::from_str_radix(msg.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                        our_stream.id.store(id, Ordering::Relaxed);
                        status(format!("DAX IQ channel {channel} stream 0x{id:08X} created"));
                        if let Some(r) = want_rate {
                            ctl.send(&format!("stream set 0x{id:08X} daxiq_rate={r}"));
                        }
                    } else if !legacy_tried {
                        // SmartSDR v1.x syntax
                        legacy_tried = true;
                        create_seq = ctl.send(&format!("stream create daxiq={channel}"));
                    } else {
                        status(format!("Could not create DAX IQ stream (error 0x{code:08X} {msg})"));
                    }
                }
            }
            'S' => {
                let Some((_, st)) = body.split_once('|') else { continue };
                let words: Vec<&str> = st.split_whitespace().collect();
                let m = kv(st);
                if words.len() >= 2 && words[0] == "slice" {
                    let sl = slices.entry(words[1].to_string()).or_default();
                    if let Some(v) = m.get("in_use") {
                        sl.in_use = *v == "1";
                    }
                    if let Some(v) = m.get("index_letter") {
                        sl.letter = v.to_string();
                    }
                    if let Some(v) = m.get("mode") {
                        sl.mode = v.to_string();
                    }
                    if let Some(f) = m.get("RF_frequency").and_then(|v| v.parse().ok()) {
                        sl.mhz = Some(f);
                    }
                    if let Some(p) = m.get("pan") {
                        sl.pan = p.to_string();
                    }
                } else if words.len() >= 3 && words[0] == "display" && words[1] == "pan" {
                    let e = pans.entry(words[2].to_string()).or_default();
                    if let Some(c) = m.get("center").and_then(|v| v.parse().ok()) {
                        e.0 = Some(c);
                    }
                    if let Some(d) = m.get("daxiq_channel").or(m.get("daxiq")).and_then(|v| v.parse().ok()) {
                        e.1 = Some(d);
                    }
                } else if words.len() >= 2
                    && ((words[0] == "daxiq" && words[1] == ch)
                        || (words[0] == "stream" && m.get("daxiq_channel") == Some(&ch.as_str())))
                {
                    if let Some(p) = m.get("pan").or(m.get("panadapter")) {
                        our_pan = Some(p.to_string());
                    }
                    // Firmware v4+ reports the sample byte order; earlier firmware is big-endian.
                    match m.get("payload_endian") {
                        Some(&"little") => our_stream.endian.store(ENDIAN_LITTLE, Ordering::Relaxed),
                        Some(&"big") => our_stream.endian.store(ENDIAN_BIG, Ordering::Relaxed),
                        _ => {}
                    }
                }
                let center = our_pan
                    .as_ref()
                    .and_then(|p| pans.get(p))
                    .and_then(|e| e.0)
                    .or_else(|| pans.values().find(|e| e.1 == Some(channel)).and_then(|e| e.0));
                if let Some(c) = center {
                    if last_center != Some(c) {
                        last_center = Some(c);
                        let _ = tx.send(SourceEvent::Center(c * 1e6));
                    }
                }
            }
            _ => {}
        }
        // Report slices on the panadapter feeding our I/Q stream whenever they change.
        let pan = our_pan.clone().or_else(|| {
            pans.iter().find(|(_, e)| e.1 == Some(channel)).map(|(id, _)| id.clone())
        });
        let list = slices_on(&slices, pan.as_deref());
        if list != last_slices {
            last_slices = list.clone();
            let _ = tx.send(SourceEvent::Slices(list));
        }
    }
    let id = our_stream.id.load(Ordering::Relaxed);
    if id != 0 {
        ctl.send(&format!("stream remove 0x{id:08X}"));
    }
}

const ENDIAN_UNKNOWN: u8 = 0;
const ENDIAN_BIG: u8 = 1;
const ENDIAN_LITTLE: u8 = 2;

#[derive(Default)]
struct StreamInfo {
    /// Our DAX IQ stream ID, 0 until the radio answers `stream create`.
    id: AtomicU32,
    endian: AtomicU8,
}

/// Guess float byte order from a payload: the wrong order yields huge, tiny or non-finite values.
fn guess_endian(payload: &[u8]) -> u8 {
    let plausible = |f: fn([u8; 4]) -> f32| {
        payload.chunks_exact(4).take(64).all(|c| {
            let v = f([c[0], c[1], c[2], c[3]]).abs();
            v.is_finite() && v < 1e3 && (v == 0.0 || v > 1e-20)
        })
    };
    match (plausible(f32::from_le_bytes), plausible(f32::from_be_bytes)) {
        (true, false) => ENDIAN_LITTLE,
        (false, true) => ENDIAN_BIG,
        _ => ENDIAN_UNKNOWN,
    }
}

fn data_loop(udp: UdpSocket, tx: Sender<SourceEvent>, stop: Arc<AtomicBool>, our_stream: Arc<StreamInfo>) {
    let mut buf = vec![0u8; 65536];
    let mut batch: Vec<Complex32> = Vec::new();
    let mut batch_rate = 0u32;
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        let n = match udp.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let Some(p) = vita::parse(&buf[..n]) else { continue };
        let Some(rate) = vita::daxiq_rate(p.packet_class) else { continue };
        let ours = our_stream.id.load(Ordering::Relaxed);
        if ours != 0 && p.stream_id != Some(ours) {
            continue;
        }
        if p.oui.is_some_and(|o| o != vita::FLEX_OUI) && !warned {
            warned = true;
            let _ = tx.send(SourceEvent::Status(format!("Unexpected VITA OUI {:06X}", p.oui.unwrap())));
        }
        if rate != batch_rate && !batch.is_empty() {
            let _ = tx.send(SourceEvent::Samples { rate: batch_rate, data: std::mem::take(&mut batch) });
        }
        batch_rate = rate;
        let mut endian = our_stream.endian.load(Ordering::Relaxed);
        if endian == ENDIAN_UNKNOWN {
            endian = guess_endian(p.payload);
            if endian == ENDIAN_UNKNOWN {
                continue;
            }
            our_stream.endian.store(endian, Ordering::Relaxed);
        }
        vita::decode_iq_f32(p.payload, endian == ENDIAN_LITTLE, &mut batch);
        if batch.len() >= (rate / 50) as usize
            && tx.send(SourceEvent::Samples { rate, data: std::mem::take(&mut batch) }).is_err()
        {
            return;
        }
    }
}
