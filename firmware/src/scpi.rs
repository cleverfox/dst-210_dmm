//! SCPI-like command server over raw TCP (port 5555, same as Rigol/Keysight LAN).
//!
//! Commands are newline-terminated, several commands may be joined with `;`.
//! Keywords accept the short (upper-case part) or the long form, case-insensitive.
//! Values are returned in base units (V, A, Ohm, F, Hz, deg C) as exact
//! decimals from the meter's display, e.g. `7.042E+02`. Overload (`OL`) is
//! returned as `9.9E+37`.
//!
//! | Command                                | Description                                  |
//! |----------------------------------------|----------------------------------------------|
//! | `*IDN?`                                | identification                               |
//! | `*RST`, `*CLS`, `*OPC?`                | clear errors (meter untouched) / same / 1    |
//! | `SYSTem:ERRor?`                        | pop error queue                              |
//! | `CONFigure:<func> [range]`             | switch the meter function (no wait)          |
//! | `CONFigure?`                           | meter function, range, resolution: `"VOLT +2.0E+03,+1.0E-01"` |
//! | `MEASure:<func>? [range]`              | switch function, wait, return a reading      |
//! | `MEASure?` / `READ?`                   | next reading in the current function         |
//! | `INITiate`, `ABORt`, `MEASure`         | accepted, no effect (sigrok sends them)      |
//! | `FETCh?`                               | latest reading without waiting               |
//! | `DATA2?`                               | secondary: AC frequency in V AC, Hz          |
//! | `DMM:DETail?`                          | value,"unit","function","display",freq       |
//! | `DMM:RAW?`                             | latest raw packet, hex                       |
//! | `DMM:MODE <name>` / `DMM:MODE?`        | send a raw mode command by name / last sent  |
//!
//! `<func>`: `VOLTage[:DC]`, `VOLTage:AC`, `RESistance`, `CONTinuity`,
//! `DIODe`, `CAPacitance`, `FREQuency`, `TEMPerature`, `CURRent[:DC]`,
//! `CURRent:AC`, and the meter specific `LIVE` (live-wire detection, reading
//! 1 = live, 0 = nothing) and `AUTO` (meter auto mode).
//! For current, a range above 0.6 A (or `MAX`) selects the 10 A input,
//! otherwise the mA input. `SENSe:FUNCtion "<func>"` is accepted as well.
//!
//! `*IDN?` and `CONFigure?` follow the Agilent 34405A, so sigrok's `scpi-dmm`
//! driver (which matches the model in `*IDN?`) drives the bridge.
//!
//! The same commands are served on the USB serial port (USB Serial/JTAG),
//! which carries the log until a client sends `*IDN?` or `*OPC?`: from then
//! on logging stops and the port is SCPI only, until reset.

use core::fmt::Write as _;

use embassy_net::{IpListenEndpoint, tcp::TcpSocket};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use esp_hal::Async;
use esp_hal::usb::usb_serial_jtag::UsbSerialJtag;
use heapless::{Deque, String, Vec};

use crate::dmm::{self, DmmError, Function, Mode, Reading, Value};

pub const SCPI_PORT: u16 = 5555;

/// Model of a supported sigrok `scpi-dmm` meter; the serial field names the bridge.
const IDN: &str = concat!("Agilent Technologies,34405A,DST210-BRIDGE,", env!("CARGO_PKG_VERSION"));

/// SCPI "overload" value.
const OVERLOAD: &str = "9.9E+37";
/// SCPI "not a number".
const NAN: &str = "9.91E+37";

/// Above this current range the 10 A input is used.
const MA_INPUT_MAX_A: f32 = 0.6;

type Response = String<512>;

#[derive(Clone, Copy)]
enum ScpiError {
    Undefined,
    Syntax,
    DataOutOfRange,
    Dmm(DmmError),
}

impl ScpiError {
    fn format(&self, out: &mut String<96>) {
        let _ = match self {
            ScpiError::Undefined => write!(out, "-113,\"Undefined header\""),
            ScpiError::Syntax => write!(out, "-102,\"Syntax error\""),
            ScpiError::DataOutOfRange => write!(out, "-222,\"Data out of range\""),
            ScpiError::Dmm(e) => write!(out, "-200,\"Execution error; {}\"", e.as_str()),
        };
    }
}

impl From<DmmError> for ScpiError {
    fn from(e: DmmError) -> Self {
        ScpiError::Dmm(e)
    }
}

struct Session {
    errors: Deque<ScpiError, 8>,
}

impl Session {
    fn push_error(&mut self, e: ScpiError) {
        if self.errors.is_full() {
            self.errors.pop_front();
        }
        let _ = self.errors.push_back(e);
    }
}

/// Match a header against a SCPI pattern such as `MEASure:VOLTage:DC?`.
/// Every node may be given in its short (upper-case) or long form.
fn header_is(header: &str, pattern: &str) -> bool {
    let header = header.strip_prefix(':').unwrap_or(header);
    let mut h_nodes = header.split(':');
    let mut p_nodes = pattern.split(':');
    loop {
        match (h_nodes.next(), p_nodes.next()) {
            (None, None) => return true,
            (Some(h), Some(p)) if node_matches(h, p) => {}
            _ => return false,
        }
    }
}

fn node_matches(h: &str, p: &str) -> bool {
    let (h, h_query) = h.strip_suffix('?').map_or((h, false), |h| (h, true));
    let (p, p_query) = p.strip_suffix('?').map_or((p, false), |p| (p, true));
    let short = &p[..p.find(|c: char| c.is_ascii_lowercase()).unwrap_or(p.len())];
    h_query == p_query && (h.eq_ignore_ascii_case(p) || h.eq_ignore_ascii_case(short))
}

fn header_any(header: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| header_is(header, p))
}

/// SCPI function names -> (mode, mode for the high current range).
const FUNCTIONS: &[(&str, &str, &str)] = &[
    ("VOLTage", "dcv", "dcv"),
    ("VOLTage:DC", "dcv", "dcv"),
    ("VOLTage:AC", "acv", "acv"),
    ("RESistance", "r", "r"),
    ("CONTinuity", "cont", "cont"),
    ("DIODe", "diode", "diode"),
    ("CAPacitance", "cap", "cap"),
    ("FREQuency", "freq", "freq"),
    ("TEMPerature", "temp", "temp"),
    ("CURRent", "dcma", "dca"),
    ("CURRent:DC", "dcma", "dca"),
    ("CURRent:AC", "acma", "aca"),
    ("LIVE", "live", "live"),
    ("AUTO", "auto", "auto"),
];

/// Find the function in `header` = `<root>:<func>[?]`, pick the input by range.
fn function_mode(header: &str, root: &str, query: bool, range: &str) -> Option<&'static Mode> {
    let (_, low, high) = FUNCTIONS.iter().find(|(f, _, _)| {
        let mut pattern: String<48> = String::new();
        let _ = write!(pattern, "{}:{}{}", root, f, if query { "?" } else { "" });
        header_is(header, &pattern)
    })?;
    mode_for_range(low, high, range)
}

/// Same lookup for a bare function name, e.g. `SENSe:FUNCtion "CURR:AC"`.
fn function_mode_by_name(name: &str, range: &str) -> Option<&'static Mode> {
    let (_, low, high) = FUNCTIONS.iter().find(|(f, _, _)| header_is(name, f))?;
    mode_for_range(low, high, range)
}

fn mode_for_range(low: &str, high: &str, range: &str) -> Option<&'static Mode> {
    let range = range.trim();
    let high_range = range.eq_ignore_ascii_case("MAX")
        || range.eq_ignore_ascii_case("MAXimum")
        || range.parse::<f32>().is_ok_and(|r| r > MA_INPUT_MAX_A);
    dmm::mode_by_name(if high_range { high } else { low })
}

/// Reading value in base units.
fn format_value(out: &mut Response, r: &Reading) {
    let _ = match (r.function, r.value) {
        (Function::Live, Value::LiveDetected) => out.write_str("1"),
        (Function::Live, Value::Overload) => out.write_str("0"),
        (_, Value::Number { mantissa, exp }) => dmm::write_sci(out, mantissa, exp),
        (_, Value::Overload) => out.write_str(OVERLOAD),
        _ => out.write_str(NAN),
    };
}

fn format_detail(out: &mut Response, r: &Reading) {
    format_value(out, r);
    let _ = write!(
        out,
        ",\"{}\",\"{}\",\"{}\",",
        r.unit.as_str(),
        r.function.scpi(),
        r.display.as_str().trim()
    );
    let _ = match r.freq_hz {
        Some(f) => write!(out, "{}", f),
        None => out.write_str(NAN),
    };
}

/// `CONFigure?` answer: `"<func> <range>,<resolution>"`, as Keysight meters
/// give it. sigrok needs it in this form and takes its display digits from
/// the resolution.
fn format_conf(out: &mut Response, r: &Reading) {
    let name = match r.function {
        Function::VoltDc => "VOLT",
        Function::CurrDc => "CURR",
        f => f.scpi(),
    };
    match r.function {
        // No range for these, and none known for LIVE
        Function::Diode | Function::Cont | Function::Live | Function::Unknown => {
            let _ = write!(out, "\"{}\"", name);
        }
        _ => {
            let exp = r.resolution_exp;
            // 4 digits plus a leading "1": full scale is about 2 * 10^(exp + 4)
            let _ = write!(out, "\"{} ", name);
            let _ = dmm::write_sci(out, 2, exp + 4);
            let _ = out.write_char(',');
            let _ = dmm::write_sci(out, 1, exp);
            let _ = out.write_char('"');
        }
    }
}

fn latest() -> Result<Reading, ScpiError> {
    dmm::latest().ok_or(ScpiError::Dmm(DmmError::NoData))
}

/// Execute one command; the response (if any) is appended to `out`.
async fn execute(session: &mut Session, cmd: &str, out: &mut Response) -> Result<(), ScpiError> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Ok(());
    }
    let (header, params) = match cmd.find(char::is_whitespace) {
        Some(i) => (&cmd[..i], cmd[i..].trim()),
        None => (cmd, ""),
    };

    // Common commands
    if header.eq_ignore_ascii_case("*IDN?") {
        let _ = out.push_str(IDN);
    } else if header.eq_ignore_ascii_case("*RST") || header.eq_ignore_ascii_case("*CLS") {
        // *RST does not change the meter function: the meter is shared with
        // its own front panel
        session.errors.clear();
    } else if header.eq_ignore_ascii_case("*OPC?") {
        let _ = out.push('1');
    } else if header_is(header, "SYSTem:ERRor?") || header_is(header, "SYSTem:ERRor:NEXT?") {
        let mut s: String<96> = String::new();
        match session.errors.pop_front() {
            Some(e) => e.format(&mut s),
            None => {
                let _ = s.push_str("0,\"No error\"");
            }
        }
        let _ = out.push_str(&s);
    }
    // Function selection
    else if header_any(header, &["CONFigure?", "SENSe:FUNCtion?", "FUNCtion?"]) {
        format_conf(out, &latest()?);
    } else if header_any(header, &["SENSe:FUNCtion", "FUNCtion"]) {
        let name = params.trim_matches(|c| c == '"' || c == '\'');
        let mode = function_mode_by_name(name, "").ok_or(ScpiError::DataOutOfRange)?;
        dmm::send_mode(mode.code).await;
    } else if let Some(mode) = function_mode(header, "CONFigure", false, params) {
        dmm::send_mode(mode.code).await;
    } else if let Some(mode) = function_mode(header, "MEASure", true, params) {
        format_value(out, &dmm::switch_mode(mode).await?);
    }
    // Readings
    else if header_any(header, &["INITiate", "ABORt", "MEASure"]) {
        // Readings stream continuously; nothing to start or stop
    } else if header_any(header, &["MEASure?", "READ?"]) {
        format_value(out, &dmm::next_reading().await?);
    } else if header_is(header, "FETCh?") {
        format_value(out, &latest()?);
    } else if header_is(header, "DATA2?") {
        let _ = match latest()?.freq_hz {
            Some(f) => write!(out, "{}", f),
            None => out.write_str(NAN),
        };
    }
    // Meter specific
    else if header_is(header, "DMM:DETail?") {
        format_detail(out, &latest()?);
    } else if header_is(header, "DMM:RAW?") {
        for (i, b) in latest()?.raw.iter().enumerate() {
            let _ = write!(out, "{}{:02X}", if i > 0 { " " } else { "" }, b);
        }
    } else if header_is(header, "DMM:MODE") {
        let mode = dmm::mode_by_name(params).ok_or(ScpiError::DataOutOfRange)?;
        dmm::send_mode(mode.code).await;
    } else if header_is(header, "DMM:MODE?") {
        let name = dmm::stats().last_cmd.and_then(dmm::mode_by_code).map_or("NONE", |m| m.name);
        let _ = out.push_str(name);
    } else {
        return Err(ScpiError::Undefined);
    }
    Ok(())
}

/// Execute a full line (possibly several `;`-separated commands).
/// Responses of several queries are joined with `;`.
async fn execute_line(session: &mut Session, line: &str, out: &mut Response) {
    for cmd in line.split(';') {
        let joined = !out.is_empty();
        if joined {
            let _ = out.push(';');
        }
        let mark = out.len();
        if let Err(e) = execute(session, cmd, out).await {
            println!("SCPI error on '{}'", cmd.trim());
            session.push_error(e);
            out.truncate(mark);
        }
        if joined && out.len() == mark {
            out.pop();
        }
    }
}

#[embassy_executor::task(pool_size = 4)]
pub async fn scpi_server(stack: embassy_net::Stack<'static>, name: &'static str) {
    let mut rx_buffer = [0u8; 512];
    let mut tx_buffer = [0u8; 1024];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        // Keep idle connections for a long time: test scripts hold them open
        socket.set_timeout(Some(Duration::from_secs(3600)));
        socket.set_keep_alive(Some(Duration::from_secs(30)));

        if let Err(e) = socket.accept(IpListenEndpoint { addr: None, port: SCPI_PORT }).await {
            println!("[{}] SCPI accept error: {:?}", name, e);
            Timer::after(Duration::from_millis(1000)).await;
            continue;
        }
        println!("[{}] SCPI client connected: {:?}", name, socket.remote_endpoint());

        let _ = handle_client(&mut socket, false).await;

        println!("[{}] SCPI client disconnected", name);
        socket.close();
        Timer::after(Duration::from_millis(50)).await;
        socket.abort();
    }
}

/// SCPI on the USB serial port. The port carries the log until the first
/// `*IDN?` / `*OPC?` line; other lines before it are ignored.
#[embassy_executor::task]
pub async fn usb_server(mut usb: UsbSerialJtag<'static, Async>) {
    loop {
        let _ = handle_client(&mut usb, true).await;
        Timer::after(Duration::from_millis(100)).await;
    }
}

/// Whether a line opens a SCPI session on the shared USB port.
fn is_session_start(line: &str) -> bool {
    let first = line.split(';').next().unwrap_or("").trim();
    first.eq_ignore_ascii_case("*IDN?") || first.eq_ignore_ascii_case("*OPC?")
}

/// Serve SCPI lines on `io`. With `gated`, lines are ignored until one starts
/// a session, which also switches the log off (the port is shared with it).
async fn handle_client<S: Read + Write>(io: &mut S, mut gated: bool) -> Result<(), S::Error> {
    let mut session = Session { errors: Deque::new() };
    let mut line: Vec<u8, 256> = Vec::new();
    let mut buf = [0u8; 128];

    loop {
        let n = io.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        for &b in &buf[..n] {
            match b {
                b'\n' => {
                    let text = core::str::from_utf8(&line).unwrap_or("");
                    if gated && is_session_start(text) {
                        crate::logging_off();
                        gated = false;
                    }
                    if !gated {
                        let mut out = Response::new();
                        execute_line(&mut session, text, &mut out).await;
                        if !out.is_empty() {
                            // A client that stops reading (closed USB port) must not hang us
                            let reply = async {
                                io.write_all(out.as_bytes()).await?;
                                io.write_all(b"\n").await?;
                                io.flush().await
                            };
                            if let Ok(r) = with_timeout(Duration::from_secs(2), reply).await {
                                r?;
                            }
                        }
                    }
                    line.clear();
                }
                b'\r' => {}
                _ => {
                    if line.push(b).is_err() {
                        // Overlong line: drop it
                        line.clear();
                        session.push_error(ScpiError::Syntax);
                    }
                }
            }
        }
    }
}
