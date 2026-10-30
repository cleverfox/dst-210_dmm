//! ESP32-C6 network bridge for the FNIRSI DST-210 multimeter
//!
//! - Listens to the meter's internal readings UART (multimeter -> display)
//! - Injects mode commands into the command UART (display -> multimeter)
//! - SCPI-like TCP server on port 5555 (Rigol/Keysight LAN style), and on the
//!   USB serial port once a client opens it with `*IDN?` / `*OPC?`
//! - HTTP server on port 80 with web UI and JSON API
//! - WiFi APSTA: own access point + connection to a configured network
//! - BLE GATT service for WiFi configuration

#![no_std]
#![no_main]

extern crate alloc;

/// Log line on the USB serial port, until the port is taken over by SCPI.
macro_rules! println {
    ($($arg:tt)*) => {
        if $crate::LOGGING.load(core::sync::atomic::Ordering::Relaxed) {
            esp_println::println!($($arg)*);
        }
    };
}

mod config;
mod dmm;
mod http;
mod scpi;

use alloc::string::ToString;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_net::{Ipv4Cidr, Runner, StackResources, StaticConfigV4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    rng::Rng,
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
    usb::usb_serial_jtag::UsbSerialJtag,
};
use esp_radio::ble::controller::BleConnector;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiModeConfig, Interface, WifiController,
    ap::AccessPointConfig, sta::StationConfig,
};
use esp_hal_dhcp_server::simple_leaser::SimpleDhcpLeaser;
use esp_hal_dhcp_server::structs::DhcpServerConfig;
use heapless::String;
use trouble_host::prelude::*;

use config::WifiConfig;

esp_bootloader_esp_idf::esp_app_desc!();

// AP configuration
pub const AP_SSID: &str = "dst210-bridge";
pub const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
const DHCP_POOL_START: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 10);
const DHCP_POOL_END: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 100);

// Default STA configuration (compile-time defaults, can be overridden via web UI)
const DEFAULT_WIFI_SSID: &str = env!("WIFI_SSID");
const DEFAULT_WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

// BLE configuration
const BLE_CONNECTIONS_MAX: usize = 1;
const BLE_L2CAP_CHANNELS_MAX: usize = 2; // Signal + ATT

/// Meter UART: 9600 8N1, 3.3 V logic on both sides of the isolator.
const DMM_BAUD: u32 = 9600;

/// BLE provisioning opens once the station has been disconnected this long
/// (or never connected since boot), and closes when it connects.
const BLE_PROVISION_DELAY: Duration = Duration::from_secs(30);

/// Log output enabled; cleared when SCPI takes over the USB serial port.
pub static LOGGING: AtomicBool = AtomicBool::new(true);

/// Stop all log output: the USB serial port is now an SCPI channel.
pub fn logging_off() {
    LOGGING.store(false, Ordering::Relaxed);
    log::set_max_level(log::LevelFilter::Off);
}

// BLE connected state
static BLE_CONNECTED: AtomicBool = AtomicBool::new(false);

// GATT Server definition for WiFi configuration
#[gatt_server]
struct BleServer {
    wifi_service: WifiConfigService,
}

/// WiFi Configuration Service
#[gatt_service(uuid = "12345678-1234-5678-1234-56789abcdef0")]
struct WifiConfigService {
    /// WiFi SSID (write-only for security)
    #[characteristic(uuid = "12345678-1234-5678-1234-56789abcdef1", write, value = [0; 32])]
    ssid: [u8; 32],
    /// WiFi Password (write-only for security)
    #[characteristic(uuid = "12345678-1234-5678-1234-56789abcdef2", write, value = [0; 64])]
    password: [u8; 64],
    /// Current STA IP Address (read-only, notify)
    #[characteristic(uuid = "12345678-1234-5678-1234-56789abcdef3", read, notify, value = [0; 16])]
    ip_address: [u8; 16],
}

// Shared WiFi configuration
pub static WIFI_CONFIG: Mutex<CriticalSectionRawMutex, WifiConfig> = Mutex::new(WifiConfig::new());
pub static STA_IP: Mutex<CriticalSectionRawMutex, Option<Ipv4Addr>> = Mutex::new(None);
pub static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // COEX (WiFi + BLE) needs more RAM
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 48 * 1024);

    println!("=== DST-210 Bridge ===");

    // Load configuration from flash
    config::init(peripherals.FLASH);
    if let Some(cfg) = config::load_wifi() {
        println!("Loaded WiFi config: SSID='{}'", cfg.ssid_str());
        *WIFI_CONFIG.lock().await = cfg;
    } else {
        println!("No saved WiFi config found");
    }

    // Meter link on UART1:
    // GPIO4 <- DMM-READINGS (isolator output, multimeter -> display)
    // GPIO5 -> COMMANDS (isolator input, wired-AND with the display MCU TX)
    let uart = Uart::new(peripherals.UART1, UartConfig::default().with_baudrate(DMM_BAUD))
        .unwrap()
        .with_rx(peripherals.GPIO4)
        .with_tx(peripherals.GPIO5)
        .into_async();
    let (dmm_rx, dmm_tx) = uart.split();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Start the meter link right away; it does not depend on the network
    spawner.spawn(dmm::rx_task(dmm_rx).unwrap());
    spawner.spawn(dmm::tx_task(dmm_tx).unwrap());
    spawner.spawn(scpi::usb_server(UsbSerialJtag::new(peripherals.USB_DEVICE).into_async()).unwrap());

    // Initialize BLE
    let connector = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    let ble_controller: ExternalController<_, 1> = ExternalController::new(connector);

    // Initialize WiFi
    let mut controller = WifiController::new(peripherals.WIFI, Default::default()).unwrap();
    let wifi_ap_device = Interface::access_point();
    let wifi_sta_device = Interface::station();

    // Network configurations
    let ap_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(AP_IP, 24),
        gateway: Some(AP_IP),
        dns_servers: Default::default(),
    });
    let sta_config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Create network stacks
    // AP: DHCP server + 2x HTTP + 2x SCPI; STA: DHCP client + 2x HTTP + 2x SCPI
    let (ap_stack, ap_runner) = embassy_net::new(
        wifi_ap_device,
        ap_config,
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );
    let (sta_stack, sta_runner) = embassy_net::new(
        wifi_sta_device,
        sta_config,
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );

    // Configure APSTA mode
    // Use saved config if valid, otherwise use compile-time defaults
    let wifi_config = WIFI_CONFIG.lock().await;
    let (ssid, password) = if wifi_config.is_valid() {
        println!("Using saved WiFi config: SSID='{}'", wifi_config.ssid_str());
        (wifi_config.ssid_str(), wifi_config.password_str())
    } else {
        println!("Using default WiFi config: SSID='{}'", DEFAULT_WIFI_SSID);
        (DEFAULT_WIFI_SSID, DEFAULT_WIFI_PASSWORD)
    };
    let ap = || AccessPointConfig::default().with_ssid(AP_SSID.try_into().unwrap());
    // A bad station config must not stop the AP, the only way to fix it
    let station = if !config::password_ok(password) {
        println!("WiFi password for '{}' is invalid (needs 8..63 characters)", ssid);
        None
    } else if password.is_empty() {
        Some(AuthenticationMethodConfig::Open)
    } else {
        password.try_into().ok().map(AuthenticationMethodConfig::WpaWpa2Personal)
    }
    .zip(ssid.try_into().ok())
    .map(|(auth, ssid)| StationConfig::default().with_ssid(ssid).with_authentication(auth));
    drop(wifi_config);

    // Applying the config also starts WiFi
    println!("Starting WiFi...");
    let applied = match station {
        Some(station) => controller
            .set_config(&WifiModeConfig::AccessPointStation(station, ap()))
            .map_err(|e| println!("WiFi station config rejected: {:?}", e))
            .is_ok(),
        None => false,
    };
    if !applied {
        println!("Starting the AP only");
        controller.set_config(&WifiModeConfig::AccessPoint(ap())).unwrap();
    }
    println!("WiFi started!");

    // Spawn all tasks
    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(ap_runner).unwrap());
    spawner.spawn(net_task(sta_runner).unwrap());
    spawner.spawn(sta_ip_monitor(sta_stack).unwrap());
    spawner.spawn(dhcp_server(ap_stack).unwrap());
    spawner.spawn(ble_task(ble_controller).unwrap());

    // Wait for AP link
    loop {
        if ap_stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("");
    println!("=== Ready! ===");
    println!("AP Mode: Connect to '{}' at http://{}", AP_SSID, AP_IP);
    println!("SCPI: port {}", scpi::SCPI_PORT);
    println!("HTTP: port {}", http::HTTP_PORT);
    println!("BLE: 'DST210-Bridge' advertises while WiFi is not connected");
    println!("");

    // Spawn server tasks for both AP and STA interfaces
    spawner.spawn(scpi::scpi_server(ap_stack, "AP").unwrap());
    spawner.spawn(scpi::scpi_server(ap_stack, "AP").unwrap());
    spawner.spawn(scpi::scpi_server(sta_stack, "STA").unwrap());
    spawner.spawn(scpi::scpi_server(sta_stack, "STA").unwrap());
    spawner.spawn(http::http_server(ap_stack, "AP").unwrap());
    spawner.spawn(http::http_server(ap_stack, "AP").unwrap());
    spawner.spawn(http::http_server(sta_stack, "STA").unwrap());
    spawner.spawn(http::http_server(sta_stack, "STA").unwrap());

    // Main loop - keep alive
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn ble_task(controller: ExternalController<BleConnector<'static>, 1>) {
    let address = Address::random([0xff, 0xe4, 0x05, 0x1a, 0x8f, 0xff]);
    println!("BLE address: {:?}", address);

    let mut resources: HostResources<ExternalController<BleConnector<'static>, 1>, DefaultPacketPool, BLE_CONNECTIONS_MAX, BLE_L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);

    let stack = stack.build();
    let mut peripheral = stack.peripheral();
    let mut runner = stack.runner();

    let mut adv_data = [0; 31];
    let adv_len = AdStructure::encode_slice(
        &[
            // Flags must come first in connectable advertising data
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(b"DST210-Bridge"),
        ],
        &mut adv_data[..],
    )
    .unwrap();

    println!("BLE: Starting advertising and GATT service");
    let server = BleServer::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: "DST210-Bridge",
        appearance: &appearance::UNKNOWN,
    }))
    .unwrap();

    let _ = join(runner.run(), async {
        let mut params = AdvertisementParameters::default();
        params.interval_min = Duration::from_millis(100);
        params.interval_max = Duration::from_millis(100);

        loop {
            wifi_down_for(BLE_PROVISION_DELAY).await;
            println!("BLE: WiFi not connected, advertising");
            match peripheral
                .advertise(
                    &params,
                    Advertisement::ConnectableScannableUndirected {
                        adv_data: &adv_data[..adv_len],
                        scan_data: &[],
                    },
                )
                .await
            {
                // Dropping the advertiser stops advertising
                Ok(adv) => match select(adv.accept(), wifi_up()).await {
                    Either::Second(()) => println!("BLE: WiFi connected, advertising stopped"),
                    Either::First(Ok(conn)) => match conn.with_attribute_server(&server) {
                        Ok(gatt_conn) => {
                            println!("BLE: Client connected");
                            BLE_CONNECTED.store(true, Ordering::Relaxed);
                            if let Err(e) = ble_gatt_events(&server, &gatt_conn).await {
                                println!("BLE GATT error: {:?}", e);
                            }
                            BLE_CONNECTED.store(false, Ordering::Relaxed);
                        }
                        Err(e) => println!("BLE GATT server error: {:?}", e),
                    },
                    Either::First(Err(e)) => println!("BLE accept error: {:?}", e),
                },
                Err(e) => {
                    println!("BLE advertising error: {:?}", e);
                    Timer::after(Duration::from_secs(1)).await;
                }
            }
        }
    })
    .await;
}

/// Wait until the station has been disconnected for `delay` without a break.
async fn wifi_down_for(delay: Duration) {
    let mut down = Duration::from_secs(0);
    while down < delay {
        if WIFI_CONNECTED.load(Ordering::Relaxed) {
            down = Duration::from_secs(0);
        }
        Timer::after(Duration::from_secs(1)).await;
        down += Duration::from_secs(1);
    }
}

async fn wifi_up() {
    while !WIFI_CONNECTED.load(Ordering::Relaxed) {
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Handle BLE GATT events
async fn ble_gatt_events<P: PacketPool>(
    server: &BleServer<'_>,
    conn: &GattConnection<'_, '_, P>,
) -> Result<(), Error> {
    let ssid_handle = server.wifi_service.ssid.handle;
    let password_handle = server.wifi_service.password.handle;
    let ip_handle = server.wifi_service.ip_address.handle;

    // Update IP address initially
    update_ip_characteristic(server, conn, ip_handle).await;

    let reason = loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break reason,
            GattConnectionEvent::Gatt { event } => {
                let mut reject = None;
                match &event {
                    GattEvent::Write(write_event) => {
                        let handle = write_event.handle();
                        // Copy the payload out: the handlers below await
                        let data: heapless::Vec<u8, 64> = write_event
                            .with_data(|_, d| heapless::Vec::from_slice(&d[..d.len().min(64)]).unwrap_or_default());
                        let data = data.as_slice();

                        if handle == ssid_handle {
                            // SSID written
                            if let Ok(ssid_str) = core::str::from_utf8(data) {
                                println!("BLE: SSID received: '{}'", ssid_str);
                                let mut wifi_config = WIFI_CONFIG.lock().await;
                                let password = wifi_config.password_str().to_string();
                                wifi_config.set_credentials(ssid_str, &password);
                            }
                        } else if handle == password_handle {
                            // Password written - save config
                            if let Ok(password_str) = core::str::from_utf8(data)
                                && !config::password_ok(password_str)
                            {
                                println!("BLE: Password rejected, needs 8..63 characters");
                                reject = Some(AttErrorCode::VALUE_NOT_ALLOWED);
                            } else if let Ok(password_str) = core::str::from_utf8(data) {
                                println!("BLE: Password received, saving config...");
                                let mut wifi_config = WIFI_CONFIG.lock().await;
                                let ssid = wifi_config.ssid_str().to_string();
                                wifi_config.set_credentials(&ssid, password_str);

                                // Save to flash
                                let config_clone = wifi_config.clone();
                                drop(wifi_config);

                                if config::save_wifi(&config_clone).is_ok() {
                                    println!("BLE: WiFi config saved! Reboot to apply.");
                                } else {
                                    println!("BLE: Failed to save WiFi config");
                                }
                            }
                        }
                    }
                    GattEvent::Read(read_event) => {
                        if read_event.handle() == ip_handle {
                            // Update IP before read
                            update_ip_characteristic(server, conn, ip_handle).await;
                        }
                    }
                    _ => {}
                }

                let reply = match reject {
                    Some(code) => event.reject(code),
                    None => event.accept(),
                };
                match reply {
                    Ok(reply) => reply.send().await,
                    Err(e) => println!("BLE: Error sending response: {:?}", e),
                }
            }
            _ => {}
        }
    };
    println!("BLE: Client disconnected: {:?}", reason);
    Ok(())
}

/// Update the IP address characteristic with current STA IP
async fn update_ip_characteristic<P: PacketPool>(
    server: &BleServer<'_>,
    _conn: &GattConnection<'_, '_, P>,
    _ip_handle: u16,
) {
    let sta_ip = STA_IP.lock().await;
    let ip_str: String<16> = if let Some(ip) = *sta_ip {
        let mut s = String::new();
        let _ = core::fmt::write(&mut s, format_args!("{}", ip));
        s
    } else {
        String::try_from("No IP").unwrap_or_default()
    };

    let mut ip_bytes = [0u8; 16];
    let bytes = ip_str.as_bytes();
    ip_bytes[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
    let _ = server.set(&server.wifi_service.ip_address, &ip_bytes);
}

#[embassy_executor::task]
async fn dhcp_server(stack: embassy_net::Stack<'static>) {
    println!("DHCP server started");
    let config = DhcpServerConfig {
        ip: AP_IP,
        lease_time: Duration::from_secs(3600),
        gateways: &[AP_IP],
        subnet: None,
        dns: &[AP_IP],
        use_captive_portal: false,
    };

    let mut leaser = SimpleDhcpLeaser {
        start: DHCP_POOL_START,
        end: DHCP_POOL_END,
        leases: Default::default(),
    };
    if let Err(e) = esp_hal_dhcp_server::run_dhcp_server(stack, config, &mut leaser).await {
        println!("DHCP server error: {:?}", e);
    }
}

#[embassy_executor::task]
async fn sta_ip_monitor(stack: embassy_net::Stack<'static>) {
    loop {
        if let Some(config) = stack.config_v4() {
            let address = config.address.address();
            let mut sta_ip = STA_IP.lock().await;
            if *sta_ip != Some(address) {
                *sta_ip = Some(address);
                println!("Got STA IP: {}", address);
            }
        } else {
            let mut sta_ip = STA_IP.lock().await;
            if sta_ip.is_some() {
                *sta_ip = None;
            }
        }
        Timer::after(Duration::from_millis(1000)).await;
    }
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    println!("WiFi connection task started");

    loop {
        {
            // Use saved config SSID if valid, otherwise use default
            let ssid: String<32> = {
                let wifi_config = WIFI_CONFIG.lock().await;
                if wifi_config.is_valid() {
                    String::try_from(wifi_config.ssid_str()).unwrap_or_default()
                } else {
                    String::try_from(DEFAULT_WIFI_SSID).unwrap_or_default()
                }
            };
            println!("Connecting to WiFi '{}'...", ssid.as_str());

            match controller.connect_async().await {
                Ok(_) => {
                    WIFI_CONNECTED.store(true, Ordering::Relaxed);
                    println!("WiFi connected to '{}'!", ssid.as_str());

                    let _ = controller.wait_for_disconnect_async().await;
                    WIFI_CONNECTED.store(false, Ordering::Relaxed);

                    println!("WiFi disconnected from '{}'", ssid.as_str());
                }
                Err(e) => {
                    println!("WiFi connection failed: {:?}", e);
                    Timer::after(Duration::from_millis(5000)).await;
                }
            }
        }
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

