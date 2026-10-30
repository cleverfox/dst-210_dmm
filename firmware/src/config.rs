//! Persistent configuration stored in the NVS data partition.
//!
//! Layout (offsets inside the partition, each record in its own 4 KiB sector):
//! - 0x0000: WiFi credentials

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use esp_bootloader_esp_idf::partitions::{self, FlashRegion};
use esp_storage::FlashStorage;

const WIFI_OFFSET: u32 = 0x0000;

// Config magic number to validate stored data
const WIFI_MAGIC: u32 = 0xC0F1_6001;

const RECORD_LEN: usize = 128;
const SECTOR_SIZE: u32 = 0x1000;

/// WiFi configuration structure (stored in flash)
#[repr(C, align(4))]
#[derive(Clone)]
pub struct WifiConfig {
    magic: u32,
    ssid: [u8; 32],
    ssid_len: u8,
    password: [u8; 64],
    password_len: u8,
    _padding: [u8; 2],
}

impl WifiConfig {
    pub const fn new() -> Self {
        Self {
            magic: 0,
            ssid: [0; 32],
            ssid_len: 0,
            password: [0; 64],
            password_len: 0,
            _padding: [0; 2],
        }
    }

    pub fn is_valid(&self) -> bool {
        self.magic == WIFI_MAGIC && self.ssid_len > 0
    }

    pub fn ssid_str(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.ssid_len as usize]).unwrap_or("")
    }

    pub fn password_str(&self) -> &str {
        core::str::from_utf8(&self.password[..self.password_len as usize]).unwrap_or("")
    }

    pub fn set_credentials(&mut self, ssid: &str, password: &str) {
        self.magic = WIFI_MAGIC;
        self.ssid_len = ssid.len().min(32) as u8;
        self.ssid[..self.ssid_len as usize].copy_from_slice(&ssid.as_bytes()[..self.ssid_len as usize]);
        self.password_len = password.len().min(64) as u8;
        self.password[..self.password_len as usize].copy_from_slice(&password.as_bytes()[..self.password_len as usize]);
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let config = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Self) };
        // A record saved without the passphrase check must not reach the WiFi driver
        if config.is_valid()
            && config.ssid_len <= 32
            && config.password_len <= 64
            && password_ok(config.password_str())
        {
            Some(config)
        } else {
            None
        }
    }
}

/// WPA2 passphrase rules: empty (open network), 8..=63 printable ASCII
/// characters, or a raw 64-digit hex PSK.
pub fn password_ok(password: &str) -> bool {
    let b = password.as_bytes();
    b.is_empty()
        || ((8..=63).contains(&b.len()) && b.iter().all(|c| (0x20..0x7f).contains(c)))
        || (b.len() == 64 && b.iter().all(u8::is_ascii_hexdigit))
}

const _: () = assert!(core::mem::size_of::<WifiConfig>() <= RECORD_LEN);

/// Flash driver, created once in [`init`].
///
/// esp-storage detects the flash size (RDID over SPI1) in `FlashStorage::new`,
/// and repeating that after ROM flash accesses returns garbage, so the driver
/// must not be recreated for every access.
static FLASH: Mutex<CriticalSectionRawMutex, RefCell<Option<FlashStorage<'static>>>> =
    Mutex::new(RefCell::new(None));

pub fn init(flash: esp_hal::peripherals::FLASH<'static>) {
    FLASH.lock(|f| *f.borrow_mut() = Some(FlashStorage::new(flash)));
}

/// Run `f` on the NVS partition.
fn with_nvs<R>(f: impl FnOnce(&mut FlashRegion<'_, '_>) -> Result<R, ()>) -> Result<R, ()> {
    FLASH.lock(|cell| {
        let mut guard = cell.borrow_mut();
        let flash = guard.as_mut().ok_or(())?;
        let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let pt = partitions::read_partition_table(flash, &mut pt_mem).map_err(|_| ())?;
        let nvs = pt
            .find_partition(partitions::PartitionType::Data(
                partitions::DataPartitionSubType::Nvs,
            ))
            .map_err(|_| ())?
            .ok_or(())?;
        f(&mut nvs.as_flash_region(flash))
    })
}

fn read_record(offset: u32, buf: &mut [u8; RECORD_LEN]) -> Result<(), ()> {
    with_nvs(|nvs| nvs.read(offset, buf).map_err(|_| ()))
}

fn write_record(offset: u32, buf: &[u8; RECORD_LEN]) -> Result<(), ()> {
    with_nvs(|nvs| {
        // Every record owns a whole sector, so it is erased without reading it back
        nvs.erase(offset, offset + SECTOR_SIZE).map_err(|_| ())?;
        nvs.write(offset, buf).map_err(|_| ())
    })
}

pub fn load_wifi() -> Option<WifiConfig> {
    let mut buf = [0u8; RECORD_LEN];
    read_record(WIFI_OFFSET, &mut buf).ok()?;
    WifiConfig::from_bytes(&buf)
}

pub fn save_wifi(config: &WifiConfig) -> Result<(), ()> {
    let mut buf = [0u8; RECORD_LEN];
    let bytes = config.as_bytes();
    buf[..bytes.len()].copy_from_slice(bytes);
    write_record(WIFI_OFFSET, &buf)
}
