//! FNIRSI DST-210 internal UART link (see `../dst-210-proto.md`).
//!
//! - multimeter -> display: readings, 12 bytes, header `5A A5`, streamed
//!   continuously. Bytes 2..6 are the raw LCD segment map, bytes 6..8 hold
//!   annunciator flags, bytes 10..11 the AC frequency in Hz.
//! - display -> multimeter: mode command, 10 bytes,
//!   `AA 55 05 <mode> 00 00 00 00 00 <sum of bytes 2..8>`. Sent once per mode
//!   change, no acknowledge: the readings stream switching mode is the only
//!   feedback.
//!
//! The bridge listens to the readings line and may inject mode commands into
//! the command line (wired-AND with the display MCU's TX).

use core::cell::RefCell;
use core::fmt::Write as _;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::Async;
use esp_hal::uart::{UartRx, UartTx};
use heapless::{String, Vec};

pub const PKT_LEN: usize = 12;
const HEADER: [u8; 2] = [0x5A, 0xA5];
const CMD_LEN: usize = 10;
const CMD_HEADER: [u8; 2] = [0xAA, 0x55];
/// Command type of a mode command (byte 2).
const CMD_TYPE_MODE: u8 = 0x05;

/// No reading for this long: the meter is considered disconnected / off.
pub const STALE_AFTER: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Functions and mode commands
// ---------------------------------------------------------------------------

/// Measurement function, as reported by the reading flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Function {
    VoltDc,
    VoltAc,
    Res,
    Cont,
    Diode,
    Cap,
    Freq,
    Temp,
    CurrDc,
    CurrAc,
    /// Live-wire detection: no unit flags at all.
    Live,
    /// Meter auto mode (function auto-detect); only used for commands.
    Auto,
    Unknown,
}

impl Function {
    /// SCPI function name (`CONFigure?` style).
    pub fn scpi(&self) -> &'static str {
        match self {
            Function::VoltDc => "VOLT:DC",
            Function::VoltAc => "VOLT:AC",
            Function::Res => "RES",
            Function::Cont => "CONT",
            Function::Diode => "DIOD",
            Function::Cap => "CAP",
            Function::Freq => "FREQ",
            Function::Temp => "TEMP",
            Function::CurrDc => "CURR:DC",
            Function::CurrAc => "CURR:AC",
            Function::Live => "LIVE",
            Function::Auto => "AUTO",
            Function::Unknown => "UNKN",
        }
    }

    /// Whether a reading of function `self` satisfies a request for `want`.
    fn satisfies(&self, want: Function) -> bool {
        match want {
            Function::Auto => true,
            // Continuity flags are not captured yet; it reports as resistance
            Function::Cont => matches!(self, Function::Res | Function::Cont),
            _ => *self == want,
        }
    }
}

/// A mode command the meter understands.
#[derive(Clone, Copy)]
pub struct Mode {
    /// Short name used by the web UI / API.
    pub name: &'static str,
    pub code: u8,
    pub function: Function,
}

pub const MODES: &[Mode] = &[
    Mode { name: "dcv", code: 0x0C, function: Function::VoltDc },
    Mode { name: "acv", code: 0x0D, function: Function::VoltAc },
    Mode { name: "r", code: 0x0B, function: Function::Res },
    Mode { name: "cont", code: 0x17, function: Function::Cont },
    Mode { name: "diode", code: 0x0E, function: Function::Diode },
    Mode { name: "cap", code: 0x0A, function: Function::Cap },
    Mode { name: "freq", code: 0x0F, function: Function::Freq },
    Mode { name: "temp", code: 0x12, function: Function::Temp },
    Mode { name: "dcma", code: 0x11, function: Function::CurrDc },
    Mode { name: "dca", code: 0x10, function: Function::CurrDc }, // 10 A input
    Mode { name: "acma", code: 0x16, function: Function::CurrAc },
    Mode { name: "aca", code: 0x15, function: Function::CurrAc }, // 10 A input
    Mode { name: "live", code: 0x13, function: Function::Live },
    Mode { name: "auto", code: 0x14, function: Function::Auto },
];

pub fn mode_by_name(name: &str) -> Option<&'static Mode> {
    MODES.iter().find(|m| m.name.eq_ignore_ascii_case(name))
}

pub fn mode_by_code(code: u8) -> Option<&'static Mode> {
    MODES.iter().find(|m| m.code == code)
}

pub fn command_packet(code: u8) -> [u8; CMD_LEN] {
    let mut p = [0u8; CMD_LEN];
    p[..2].copy_from_slice(&CMD_HEADER);
    p[2] = CMD_TYPE_MODE;
    p[3] = code;
    p[9] = p[2..9].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    p
}

// ---------------------------------------------------------------------------
// Reading decoder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    V,
    A,
    Ohm,
    F,
    Hz,
    DegC,
    None,
}

impl Unit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Unit::V => "V",
            Unit::A => "A",
            Unit::Ohm => "Ohm",
            Unit::F => "F",
            Unit::Hz => "Hz",
            Unit::DegC => "degC",
            Unit::None => "",
        }
    }
}

/// What the LCD shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Value {
    /// Exact decimal in base units: `mantissa * 10^exp` (e.g. V, not mV).
    Number { mantissa: i32, exp: i8 },
    /// `OL`: overload, open circuit, or live-wire mode idle.
    Overload,
    /// `LIvE`: live-wire mode, live wire detected.
    LiveDetected,
    /// Segment pattern that is not a number or a known word.
    Invalid,
}

/// A decoded readings packet.
#[derive(Clone)]
pub struct Reading {
    pub raw: [u8; PKT_LEN],
    /// LCD text, e.g. "-1.2345", " .0L " (`i` shows as `1`, `v` as `u`).
    pub display: String<16>,
    pub value: Value,
    /// SI prefix shown on the LCD ("", "M", "k", "m", "u", "n").
    pub prefix: &'static str,
    /// Display resolution in base units, `10^resolution_exp`: the last digit
    /// position. Known for `OL` too, which keeps the decimal point.
    pub resolution_exp: i8,
    pub unit: Unit,
    pub function: Function,
    pub dc: bool,
    pub ac: bool,
    /// Byte 2 bit 2: auto-range (likely).
    pub auto_range: bool,
    /// Byte 7 bit 3: meter in auto (function detect) mode.
    pub auto_mode: bool,
    /// Byte 6 bit 7: diode symbol.
    pub diode: bool,
    /// AC frequency in Hz, secondary display in V AC.
    pub freq_hz: Option<u16>,
    pub seq: u32,
    pub at_ms: u64,
}

// Segment bits: a=0x01 b=0x02 c=0x04 d=0x08 e=0x10 f=0x20 g=0x40
const GLYPHS: &[(u8, char)] = &[
    (0x3F, '0'),
    (0x06, '1'),
    (0x5B, '2'),
    (0x4F, '3'),
    (0x66, '4'),
    (0x6D, '5'),
    (0x7D, '6'),
    (0x07, '7'),
    (0x7F, '8'),
    (0x6F, '9'),
    (0x00, ' '),
    (0x38, 'L'),
    (0x79, 'E'),
    (0x71, 'F'),
    (0x76, 'H'),
    (0x73, 'P'),
    (0x77, 'A'),
    (0x39, 'C'),
    (0x3E, 'U'),
    (0x7C, 'b'),
    (0x5E, 'd'),
    (0x54, 'n'),
    (0x5C, 'o'),
    (0x50, 'r'),
    (0x78, 't'),
    (0x1C, 'u'),
    (0x40, '-'),
];

fn glyph(hi: u8, lo: u8) -> char {
    let mut m = 0u8;
    m |= if hi & 8 != 0 { 0x01 } else { 0 }; // a
    m |= if hi & 4 != 0 { 0x20 } else { 0 }; // f
    m |= if hi & 2 != 0 { 0x10 } else { 0 }; // e
    m |= if lo & 8 != 0 { 0x02 } else { 0 }; // b
    m |= if lo & 4 != 0 { 0x40 } else { 0 }; // g
    m |= if lo & 2 != 0 { 0x04 } else { 0 }; // c
    m |= if lo & 1 != 0 { 0x08 } else { 0 }; // d
    GLYPHS.iter().find(|(s, _)| *s == m).map_or('?', |&(_, c)| c)
}

fn bit(p: &[u8; PKT_LEN], byte: usize, bit: u8) -> bool {
    p[byte] & (1 << bit) != 0
}

/// LCD text. Digit k uses the high nibble of byte k+1 and the low nibble of
/// byte k+2; the high nibble bit 0 is the DP before the digit (minus sign
/// for the first digit). Byte 2 bit 3 is the leading "1", byte 8 bit 7 the
/// DP after it.
fn decode_display(p: &[u8; PKT_LEN]) -> String<16> {
    let d = &p[2..7];
    let mut s = String::new();
    if d[0] & 0x10 != 0 {
        let _ = s.push('-');
    }
    if d[0] & 0x08 != 0 {
        let _ = s.push('1');
    }
    if p[8] & 0x80 != 0 {
        let _ = s.push('.');
    }
    for k in 1..5 {
        let (hi, lo) = (d[k - 1] >> 4, d[k] & 0x0F);
        if k > 1 && hi & 1 != 0 {
            let _ = s.push('.');
        }
        let _ = s.push(glyph(hi, lo));
    }
    s
}

/// Parse the LCD text into (mantissa, decimals), or a status word.
fn parse_display(s: &str) -> Value {
    let word: String<16> = s.chars().filter(|&c| c != '.' && c != ' ').collect();
    match word.as_str() {
        "0L" | "-0L" => return Value::Overload,
        "L1uE" => return Value::LiveDetected,
        _ => {}
    }
    let mut mantissa: i32 = 0;
    let mut decimals: i8 = 0;
    let mut seen_dp = false;
    let mut neg = false;
    let mut any_digit = false;
    for c in s.chars() {
        match c {
            '-' if !any_digit && !neg => neg = true,
            '.' if !seen_dp => seen_dp = true,
            '0'..='9' | ' ' => {
                // A blank digit (e.g. the half digit, or "-.7051") counts as 0
                mantissa = mantissa * 10 + c.to_digit(10).unwrap_or(0) as i32;
                any_digit |= c != ' ';
                if seen_dp {
                    decimals += 1;
                }
            }
            _ => return Value::Invalid,
        }
    }
    if !any_digit {
        return Value::Invalid;
    }
    Value::Number { mantissa: if neg { -mantissa } else { mantissa }, exp: -decimals }
}

/// Digit positions after the decimal point, counted like [`parse_display`].
fn display_decimals(s: &str) -> i8 {
    s.split_once('.').map_or(0, |(_, after)| after.chars().filter(|&c| c != '.').count() as i8)
}

impl Reading {
    pub fn decode(raw: &[u8; PKT_LEN], seq: u32, at_ms: u64) -> Self {
        let p = raw;
        let display = decode_display(p);

        let (prefix, prefix_exp) = if bit(p, 7, 2) {
            ("M", 6)
        } else if bit(p, 6, 6) {
            ("k", 3)
        } else if bit(p, 7, 0) {
            ("m", -3)
        } else if bit(p, 6, 4) {
            ("u", -6)
        } else if bit(p, 6, 5) {
            ("n", -9)
        } else {
            ("", 0)
        };
        let unit = if bit(p, 8, 1) {
            Unit::V
        } else if bit(p, 8, 0) {
            Unit::A
        } else if bit(p, 7, 5) {
            Unit::Ohm
        } else if bit(p, 7, 4) {
            Unit::F
        } else if bit(p, 8, 2) {
            Unit::Hz
        } else if bit(p, 8, 5) {
            Unit::DegC
        } else {
            Unit::None
        };
        let dc = bit(p, 2, 1);
        let ac = bit(p, 2, 0);

        let value = match parse_display(&display) {
            Value::Number { mantissa, exp } => Value::Number { mantissa, exp: exp + prefix_exp },
            v => v,
        };

        let resolution_exp = prefix_exp - display_decimals(&display);

        let function = match unit {
            Unit::V if ac => Function::VoltAc,
            Unit::V if dc => Function::VoltDc,
            Unit::V => Function::Diode, // diode: V without AC/DC
            Unit::A if ac => Function::CurrAc,
            Unit::A => Function::CurrDc,
            Unit::Ohm => Function::Res,
            Unit::F => Function::Cap,
            Unit::Hz => Function::Freq,
            Unit::DegC => Function::Temp,
            Unit::None if matches!(value, Value::Overload | Value::LiveDetected) => Function::Live,
            Unit::None => Function::Unknown,
        };

        Self {
            raw: *raw,
            display,
            value,
            prefix,
            resolution_exp,
            unit,
            function,
            dc,
            ac,
            auto_range: bit(p, 2, 2),
            auto_mode: bit(p, 7, 3),
            diode: bit(p, 6, 7),
            freq_hz: (ac && unit == Unit::V).then(|| u16::from_be_bytes([p[10], p[11]])),
            seq,
            at_ms,
        }
    }
}

/// Write `mantissa * 10^exp` in scientific notation without rounding,
/// e.g. (7042, -1) -> "7.042E+02".
pub fn write_sci(out: &mut impl core::fmt::Write, mantissa: i32, exp: i8) -> core::fmt::Result {
    if mantissa == 0 {
        return out.write_str("0.0E+00");
    }
    let mut digits: String<12> = String::new();
    let _ = write!(digits, "{}", mantissa.unsigned_abs());
    let e = exp as i32 + digits.len() as i32 - 1;
    if mantissa < 0 {
        out.write_char('-')?;
    }
    out.write_str(&digits[..1])?;
    if digits.len() > 1 {
        out.write_char('.')?;
        out.write_str(&digits[1..])?;
    }
    write!(out, "E{}{:02}", if e < 0 { '-' } else { '+' }, e.unsigned_abs())
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct State {
    last: Option<[u8; PKT_LEN]>,
    seq: u32,
    at_ms: u64,
    /// Last mode command seen being sent by the bridge.
    last_cmd: Option<u8>,
    cmds_sent: u32,
    rx_errors: u32,
}

static STATE: Mutex<CriticalSectionRawMutex, RefCell<State>> = Mutex::new(RefCell::new(State {
    last: None,
    seq: 0,
    at_ms: 0,
    last_cmd: None,
    cmds_sent: 0,
    rx_errors: 0,
}));

/// Mode command codes waiting to be sent.
static COMMANDS: Channel<CriticalSectionRawMutex, u8, 4> = Channel::new();

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DmmError {
    /// No readings from the meter.
    NoData,
    /// The meter did not switch to the requested function.
    ModeTimeout,
}

impl DmmError {
    pub fn as_str(&self) -> &'static str {
        match self {
            DmmError::NoData => "no data from meter",
            DmmError::ModeTimeout => "meter did not switch function",
        }
    }
}

pub struct Stats {
    pub packets: u32,
    pub cmds_sent: u32,
    pub last_cmd: Option<u8>,
    pub rx_errors: u32,
}

pub fn stats() -> Stats {
    STATE.lock(|s| {
        let s = s.borrow();
        Stats { packets: s.seq, cmds_sent: s.cmds_sent, last_cmd: s.last_cmd, rx_errors: s.rx_errors }
    })
}

/// Latest reading, if any arrived recently.
pub fn latest() -> Option<Reading> {
    let (raw, seq, at_ms) = STATE.lock(|s| {
        let s = s.borrow();
        s.last.map(|raw| (raw, s.seq, s.at_ms))
    })?;
    let fresh = Instant::now().as_millis().saturating_sub(at_ms) <= STALE_AFTER.as_millis();
    fresh.then(|| Reading::decode(&raw, seq, at_ms))
}

fn seq() -> u32 {
    STATE.lock(|s| s.borrow().seq)
}

/// Wait for a reading that arrives after this call.
pub async fn next_reading() -> Result<Reading, DmmError> {
    let start = seq();
    let deadline = Instant::now() + STALE_AFTER;
    loop {
        if let Some(r) = latest().filter(|r| r.seq != start) {
            return Ok(r);
        }
        if Instant::now() > deadline {
            return Err(DmmError::NoData);
        }
        Timer::after(Duration::from_millis(5)).await;
    }
}

/// Queue a mode command.
pub async fn send_mode(code: u8) {
    COMMANDS.send(code).await;
}

/// Switch the meter to `mode` and wait until two consecutive readings report
/// the requested function.
pub async fn switch_mode(mode: &Mode) -> Result<Reading, DmmError> {
    send_mode(mode.code).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut matched = 0;
    loop {
        let r = next_reading().await?;
        if r.function.satisfies(mode.function) {
            matched += 1;
            if matched >= 2 {
                return Ok(r);
            }
        } else {
            matched = 0;
        }
        if Instant::now() > deadline {
            return Err(DmmError::ModeTimeout);
        }
    }
}

// ---------------------------------------------------------------------------
// UART tasks
// ---------------------------------------------------------------------------

/// Receives the readings stream and keeps the latest packet.
#[embassy_executor::task]
pub async fn rx_task(mut rx: UartRx<'static, Async>) {
    println!("DMM: readings receiver started");
    let mut buf: Vec<u8, 64> = Vec::new();
    let mut chunk = [0u8; 32];
    loop {
        let n = match rx.read_async(&mut chunk).await {
            Ok(n) => n,
            Err(e) => {
                println!("DMM: UART RX error: {:?}", e);
                STATE.lock(|s| s.borrow_mut().rx_errors += 1);
                buf.clear();
                continue;
            }
        };
        for &b in &chunk[..n] {
            if buf.push(b).is_err() {
                buf.clear();
            }
        }
        // Sync on the header, take fixed-size packets
        loop {
            let Some(i) = buf.windows(2).position(|w| w == HEADER) else {
                // Keep a trailing 5A, it may be the first header byte
                let keep = buf.last() == Some(&HEADER[0]);
                buf.clear();
                if keep {
                    let _ = buf.push(HEADER[0]);
                }
                break;
            };
            if buf.len() - i < PKT_LEN {
                drain_front(&mut buf, i);
                break;
            }
            let mut pkt = [0u8; PKT_LEN];
            pkt.copy_from_slice(&buf[i..i + PKT_LEN]);
            drain_front(&mut buf, i + PKT_LEN);
            STATE.lock(|s| {
                let mut s = s.borrow_mut();
                s.last = Some(pkt);
                s.seq = s.seq.wrapping_add(1);
                s.at_ms = Instant::now().as_millis();
            });
        }
    }
}

fn drain_front(buf: &mut Vec<u8, 64>, n: usize) {
    let rest = buf.len() - n;
    buf.copy_within(n.., 0);
    buf.truncate(rest);
}

/// Sends queued mode commands.
#[embassy_executor::task]
pub async fn tx_task(mut tx: UartTx<'static, Async>) {
    println!("DMM: command sender started");
    loop {
        let code = COMMANDS.receive().await;
        let pkt = command_packet(code);
        let mut sent = 0;
        while sent < pkt.len() {
            match tx.write_async(&pkt[sent..]).await {
                Ok(n) => sent += n,
                Err(_) => break,
            }
        }
        let _ = tx.flush_async().await;
        println!("DMM: sent mode command {:02X?}", pkt);
        STATE.lock(|s| {
            let mut s = s.borrow_mut();
            s.last_cmd = Some(code);
            s.cmds_sent = s.cmds_sent.wrapping_add(1);
        });
    }
}
