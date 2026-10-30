//! HTTP server: web UI, JSON API and WiFi configuration.
//!
//! - `GET  /`              web UI (static page, data via the API)
//! - `GET  /api/measure`   latest reading as JSON
//! - `GET  /api/status`    network and link status
//! - `POST /api/mode`      `mode=dcv|acv|r|cont|diode|cap|freq|temp|dcma|dca|acma|aca|live|auto`
//! - `POST /api/wifi`      `ssid=..&password=..`
//!
//! Requests from other web pages are refused (see [`request_allowed`]).

use core::fmt::Write as _;
use core::sync::atomic::Ordering;

use embassy_net::{IpListenEndpoint, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write;
use heapless::String;

use crate::dmm::{self, Value};
use crate::scpi::SCPI_PORT;
use crate::{AP_IP, AP_SSID, STA_IP, WIFI_CONFIG, WIFI_CONNECTED, config};

pub const HTTP_PORT: u16 = 80;

type Json = String<1024>;

#[embassy_executor::task(pool_size = 4)]
pub async fn http_server(stack: embassy_net::Stack<'static>, name: &'static str) {
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 2048];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(30)));

        if let Err(e) = socket.accept(IpListenEndpoint { addr: None, port: HTTP_PORT }).await {
            println!("[{}] HTTP accept error: {:?}", name, e);
            Timer::after(Duration::from_millis(1000)).await;
            continue;
        }

        let _ = handle_http_request(&mut socket).await;

        socket.close();
        Timer::after(Duration::from_millis(20)).await;
        socket.abort();
    }
}

async fn handle_http_request(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut buffer = [0u8; 1024];
    let mut pos = 0;
    let mut body_start = None;

    // Read headers and, if present, a body of Content-Length bytes
    loop {
        if let Some(start) = body_start {
            let request = core::str::from_utf8(&buffer[..start]).unwrap_or("");
            let need = start + content_length(request).min(buffer.len() - start);
            if pos >= need {
                break;
            }
        }
        if pos >= buffer.len() {
            break;
        }
        match socket.read(&mut buffer[pos..]).await? {
            0 => break,
            len => pos += len,
        }
        if body_start.is_none() {
            body_start = buffer[..pos]
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4);
        }
    }

    let Some(start) = body_start else {
        return send(socket, "400 Bad Request", "text/plain", b"Bad Request").await;
    };
    let head = core::str::from_utf8(&buffer[..start]).unwrap_or("");
    let body = core::str::from_utf8(&buffer[start..pos]).unwrap_or("");

    let (method, path) = parse_request_line(head);

    if let Err(e) = request_allowed(method, head) {
        println!("HTTP: refused {} {}: {}", method, path, e);
        return send(socket, "403 Forbidden", "text/plain", e.as_bytes()).await;
    }

    match (method, path) {
        ("GET", "/") => send(socket, "200 OK", "text/html", INDEX_HTML.as_bytes()).await,
        ("GET", "/api/measure") => {
            let json = measure_json();
            send(socket, "200 OK", "application/json", json.as_bytes()).await
        }
        ("GET", "/api/status") => {
            let json = status_json().await;
            send(socket, "200 OK", "application/json", json.as_bytes()).await
        }
        ("POST", "/api/mode") => {
            let json = match form_value(body, "mode").and_then(dmm::mode_by_name) {
                Some(mode) => {
                    dmm::send_mode(mode.code).await;
                    result_json(Ok(()))
                }
                None => result_json(Err("unknown mode")),
            };
            send(socket, "200 OK", "application/json", json.as_bytes()).await
        }
        ("POST", "/api/wifi") => handle_wifi_config(socket, body).await,
        _ => send(socket, "404 Not Found", "text/plain", b"Not Found").await,
    }
}

async fn send(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    let mut head: String<160> = String::new();
    let _ = write!(
        head,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body).await?;
    socket.flush().await
}

fn parse_request_line(request: &str) -> (&str, &str) {
    let first_line = request.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    (method, path.split('?').next().unwrap_or(path))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

fn content_length(head: &str) -> usize {
    header(head, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Cross-site request protection, the device has no login.
///
/// - POST needs an `X-Requested-With` header. A page on another site cannot
///   add it without a CORS preflight, which this server never grants, so a
///   forged form post or fetch never reaches the handlers.
/// - `Host` must be an IP address. DNS rebinding (an attacker's domain
///   pointed at the device, making its page same-origin) always arrives with
///   that domain name in `Host`. Clients without a `Host` header are not
///   browsers and are let through.
fn request_allowed(method: &str, head: &str) -> Result<(), &'static str> {
    if let Some(host) = header(head, "host") {
        // Strip the port; `[..]` is IPv6, which the device does not serve
        let addr = host.rsplit_once(':').map_or(host, |(a, _)| a);
        if addr.parse::<core::net::Ipv4Addr>().is_err() {
            return Err("Host must be an IP address");
        }
    }
    if method != "GET" && header(head, "x-requested-with").is_none() {
        return Err("X-Requested-With header required");
    }
    Ok(())
}

/// Raw (still url-encoded) value of a form field.
fn form_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn measure_json() -> Json {
    let mut json = Json::new();
    let Some(r) = dmm::latest() else {
        let _ = write!(json, "{{\"ok\":false,\"error\":\"{}\"}}", dmm::DmmError::NoData.as_str());
        return json;
    };
    let _ = json.push_str("{\"ok\":true,\"value\":");
    let _ = match r.value {
        Value::Number { mantissa, exp } => dmm::write_sci(&mut json, mantissa, exp),
        _ => json.write_str("null"),
    };
    let _ = write!(
        json,
        ",\"overload\":{},\"live\":{},\"display\":\"{}\",\"prefix\":\"{}\",\"unit\":\"{}\",\"function\":\"{}\",\
         \"dc\":{},\"ac\":{},\"auto_range\":{},\"auto_mode\":{},\"diode\":{},\"freq_hz\":",
        r.value == Value::Overload,
        r.value == Value::LiveDetected,
        r.display.as_str(),
        r.prefix,
        r.unit.as_str(),
        r.function.scpi(),
        r.dc,
        r.ac,
        r.auto_range,
        r.auto_mode,
        r.diode,
    );
    let _ = match r.freq_hz {
        Some(f) => write!(json, "{}", f),
        None => json.write_str("null"),
    };
    let _ = json.push_str(",\"raw\":\"");
    for (i, b) in r.raw.iter().enumerate() {
        let _ = write!(json, "{}{:02X}", if i > 0 { " " } else { "" }, b);
    }
    let _ = write!(
        json,
        "\",\"seq\":{},\"age_ms\":{}}}",
        r.seq,
        Instant::now().as_millis().saturating_sub(r.at_ms)
    );
    json
}

async fn status_json() -> Json {
    let sta_ip = *STA_IP.lock().await;
    let ssid: String<32> = {
        let cfg = WIFI_CONFIG.lock().await;
        String::try_from(cfg.ssid_str()).unwrap_or_default()
    };
    let stats = dmm::stats();
    let mut json = Json::new();
    let _ = write!(
        json,
        "{{\"uptime_ms\":{},\"ap_ssid\":\"{}\",\"ap_ip\":\"{}\",\"sta_ssid\":\"{}\",\"wifi_connected\":{},\"scpi_port\":{},\
         \"packets\":{},\"rx_errors\":{},\"cmds_sent\":{},\"last_cmd\":\"{}\",\"sta_ip\":",
        Instant::now().as_millis(),
        AP_SSID,
        AP_IP,
        ssid.as_str(),
        WIFI_CONNECTED.load(Ordering::Relaxed),
        SCPI_PORT,
        stats.packets,
        stats.rx_errors,
        stats.cmds_sent,
        stats.last_cmd.and_then(dmm::mode_by_code).map_or("", |m| m.name),
    );
    let _ = match sta_ip {
        Some(ip) => write!(json, "\"{}\"}}", ip),
        None => write!(json, "null}}"),
    };
    json
}

fn result_json(r: Result<(), &str>) -> Json {
    let mut json = Json::new();
    let _ = match r {
        Ok(_) => write!(json, "{{\"ok\":true}}"),
        Err(e) => write!(json, "{{\"ok\":false,\"error\":\"{}\"}}", e),
    };
    json
}

fn url_decode(input: &str, output: &mut [u8]) -> usize {
    let mut i = 0;
    let mut o = 0;
    let bytes = input.as_bytes();

    while i < bytes.len() && o < output.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                hex_char_to_val(bytes[i + 1]),
                hex_char_to_val(bytes[i + 2]),
            ) {
                output[o] = (h << 4) | l;
                o += 1;
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            output[o] = b' ';
            o += 1;
            i += 1;
            continue;
        }
        output[o] = bytes[i];
        o += 1;
        i += 1;
    }
    o
}

fn hex_char_to_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

async fn handle_wifi_config(
    socket: &mut TcpSocket<'_>,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let json = result_json(save_wifi_form(body).await);
    send(socket, "200 OK", "application/json", json.as_bytes()).await
}

async fn save_wifi_form(body: &str) -> Result<(), &'static str> {
    let mut ssid_buf = [0u8; 32];
    let mut password_buf = [0u8; 64];
    let ssid_len = form_value(body, "ssid").map_or(0, |v| url_decode(v, &mut ssid_buf));
    let password_len = form_value(body, "password").map_or(0, |v| url_decode(v, &mut password_buf));

    let (Ok(ssid), Ok(password)) = (
        core::str::from_utf8(&ssid_buf[..ssid_len]),
        core::str::from_utf8(&password_buf[..password_len]),
    ) else {
        return Err("invalid encoding");
    };
    if ssid.is_empty() {
        return Err("SSID is required");
    }
    if !config::password_ok(password) {
        return Err("password must be empty or 8..63 characters");
    }

    println!("Saving WiFi config: SSID='{}'", ssid);
    let config_clone = {
        let mut wifi_config = WIFI_CONFIG.lock().await;
        wifi_config.set_credentials(ssid, password);
        wifi_config.clone()
    };
    config::save_wifi(&config_clone).map_err(|_| {
        println!("Failed to save WiFi config");
        "failed to save config"
    })?;
    println!("WiFi config saved to flash");
    Ok(())
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>DST-210 Bridge</title><meta name='viewport' content='width=device-width,initial-scale=1'>
<style>
body{font-family:sans-serif;margin:16px;background:#1a1a2e;color:#eee}
h1{color:#e94560;margin:0 0 4px}
.card{background:#16213e;padding:16px;border-radius:8px;margin:12px 0}
.val{font:700 3em monospace;color:#7fffd4}
.ovr{color:#ff5252}
.sec{font:700 1.4em monospace;color:#9ad}
.dim{color:#888;font-size:14px}
input{padding:8px;border:none;border-radius:4px;background:#0f3460;color:#fff}
button{background:#e94560;color:#fff;border:none;padding:8px 14px;border-radius:4px;cursor:pointer;margin:4px 4px 4px 0}
button.on{background:#7fffd4;color:#000}
code{background:#0f3460;padding:2px 6px;border-radius:4px}
#msg{min-height:1.2em}
</style></head><body>
<h1>DST-210 Bridge</h1>
<div class='dim' id='netinfo'></div>
<div class='card'><div class='dim' id='func'></div><div class='val' id='val'>---</div>
<div class='sec' id='sec'></div><div class='dim' id='flags'></div><div class='dim'><code id='raw'></code></div></div>
<div class='card'><h3>Function</h3><div id='modes'></div>
<p class='dim'>Commands are injected into the meter's command line. The meter's own display keeps its selected function.</p>
<div id='msg'></div></div>
<div class='card'><h3>WiFi</h3>
<form id='wifi' onsubmit='saveWifi(event)'>
<p><input name='ssid' id='ssid' placeholder='SSID'> <input name='password' type='password' placeholder='Password' minlength='8' maxlength='64'>
<button type='submit'>Save</button></p></form>
<p class='dim' id='wmsg'>Restart the device after saving.</p></div>
<div class='card dim'>SCPI: <code>MEAS:VOLT:DC?</code>, <code>MEAS:RES?</code>, <code>CONF:VOLT:AC</code>, <code>READ?</code>,
<code>DATA2?</code>, <code>CONF?</code>, <code>DMM:DET?</code> &mdash; TCP port <span id='port'></span></div>
<script>
const MODES=[['dcv','V DC'],['acv','V AC'],['r','\u03a9'],['cont','Cont'],['diode','Diode'],['cap','Cap'],['freq','Hz'],
 ['temp','Temp'],['dcma','mA DC'],['acma','mA AC'],['dca','10A DC'],['aca','10A AC'],['live','Live'],['auto','Auto']];
const UNIT={Ohm:'\u03a9',degC:'\u00b0C'};
modes.innerHTML=MODES.map(m=>"<button id='m_"+m[0]+"' onclick=\"setMode('"+m[0]+"')\">"+m[1]+"</button>").join('');
async function post(url,body){return (await fetch(url,{method:'POST',body:body,
 headers:{'Content-Type':'application/x-www-form-urlencoded','X-Requested-With':'fetch'}})).json()}
async function setMode(m){msg.textContent='...';
 try{const r=await post('/api/mode','mode='+m);
 msg.textContent=r.ok?'Sent':'Error: '+r.error}catch(e){msg.textContent='Error: '+e}}
async function saveWifi(ev){ev.preventDefault();wmsg.textContent='...';
 try{const r=await post('/api/wifi',new URLSearchParams(new FormData(wifi)).toString());
 wmsg.textContent=r.ok?'Saved. Restart the device to apply.':'Error: '+r.error}catch(e){wmsg.textContent='Error: '+e}}
function lcd(d){let t=d.display.trim();if(t.startsWith('-.'))t='-0'+t.slice(1);else if(t.startsWith('.'))t='0'+t;return t}
async function tick(){
 try{const d=await (await fetch('/api/measure')).json();
 if(!d.ok){val.textContent='---';val.className='val';func.textContent=d.error;sec.textContent='';flags.textContent='';raw.textContent=''}
 else{
  let t;if(d.function=='LIVE')t=d.live?'LIVE':'---';else if(d.overload)t='OL';else t=lcd(d)+' '+(d.prefix=='u'?'\u00b5':d.prefix)+(UNIT[d.unit]||d.unit);
  val.textContent=t;val.className='val'+(d.overload||d.live?' ovr':'');
  func.textContent=d.function;
  sec.textContent=d.freq_hz!==null?d.freq_hz+' Hz':'';
  flags.textContent=['dc','ac','auto_range','auto_mode','diode'].filter(k=>d[k]).join(' ');
  raw.textContent=d.raw}
 }catch(e){}
 setTimeout(tick,300)}
async function net(){try{const s=await (await fetch('/api/status')).json();
 netinfo.textContent='STA '+(s.sta_ip||'-')+' ('+s.sta_ssid+') | AP '+s.ap_ssid+' '+s.ap_ip+' | packets '+s.packets+' | commands sent '+s.cmds_sent;
 port.textContent=s.scpi_port;
 for(const m of MODES)document.getElementById('m_'+m[0]).className=(m[0]==s.last_cmd?'on':'');
 if(!ssid.value)ssid.value=s.sta_ssid}catch(e){}
 setTimeout(net,2000)}
net();tick();
</script></body></html>"#;
