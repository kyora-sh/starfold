//! Where Intel's oneVPL runtime is. It ships inside the graphics driver, in a DriverStore
//! folder that is not on the DLL search path; the adapter's driver names that folder in its
//! software registry key (`DriverStorePathForVPL`). This reads it the way Intel's own
//! dispatcher does (libvpl v2.17.0, `libvpl/src/windows/mfx_driver_store_loader.cpp`): the
//! present display-class devices from CfgMgr32, the one whose instance ID names vendor 8086
//! and the adapter's PCI device, then its software key. CfgMgr32 and the registry calls are
//! loaded at runtime from System32, as the dispatcher loads them. There is no search-path
//! fallback: a driver that names no folder has no runtime here.

use super::{intel_device, Adapter};
use crate::winlib::system as load_system;
use std::ffi::c_void;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

/// The runtime's file name, in its DriverStore folder.
const RUNTIME: &str = "libmfx64-gen.dll";
const VALUE: &str = "DriverStorePathForVPL";
/// GUID_DEVCLASS_DISPLAY, as StringFromGUID2 writes it.
const DISPLAY_CLASS: &str = "{4D36E968-E325-11CE-BFC1-08002BE10318}";
const CM_GETIDLIST_FILTER_PRESENT: u32 = 0x100;
const CM_GETIDLIST_FILTER_CLASS: u32 = 0x200;
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
const CM_REGISTRY_SOFTWARE: u32 = 1;
const REG_DISPOSITION_OPEN_EXISTING: u32 = 1;
const KEY_READ: u32 = 0x20019;
const CR_SUCCESS: u32 = 0;
const CR_BUFFER_SMALL: u32 = 0x1a;
const REG_SZ: u32 = 1;
const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;
const MAX_ADAPTERS: u32 = 16;
/// The longest folder path read, in UTF-16 units.
const MAX_PATH_UNITS: usize = 1024;

type Hkey = *mut c_void;
type GetListSize = unsafe extern "system" fn(*mut u32, *const u16, u32) -> u32;
type GetList = unsafe extern "system" fn(*const u16, *mut u16, u32, u32) -> u32;
type LocateDevNode = unsafe extern "system" fn(*mut u32, *const u16, u32) -> u32;
type OpenDevNodeKey = unsafe extern "system" fn(u32, u32, u32, u32, *mut Hkey, u32) -> u32;
type RegQueryValueEx =
    unsafe extern "system" fn(Hkey, *const u16, *mut u32, *mut u32, *mut u8, *mut u32) -> i32;
type RegCloseKey = unsafe extern "system" fn(Hkey) -> i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The Intel hardware adapters DXGI lists.
pub fn intel_adapters() -> Result<Vec<Adapter>, String> {
    unsafe {
        let f: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1 failed: {e}"))?;
        let mut v = Vec::new();
        for i in 0..MAX_ADAPTERS {
            let Ok(a) = f.EnumAdapters1(i) else {
                break;
            };
            let Ok(d) = a.GetDesc1() else {
                continue;
            };
            if d.VendorId != super::ffi::VENDOR_INTEL || d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0 {
                continue;
            }
            let n = d
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(d.Description.len());
            v.push(Adapter {
                luid: ((d.AdapterLuid.HighPart as u32 as u64) << 32) | d.AdapterLuid.LowPart as u64,
                device_id: d.DeviceId,
                name: String::from_utf16_lossy(&d.Description[..n]),
            });
        }
        Ok(v)
    }
}

/// The full path of the runtime the driver of Intel PCI device `device_id` installed.
pub fn driver_store_runtime(device_id: u32) -> Result<String, String> {
    let dir = unsafe { driver_store_dir(device_id)? };
    let sep = if dir.ends_with('\\') || dir.ends_with('/') {
        ""
    } else {
        "\\"
    };
    Ok(format!("{dir}{sep}{RUNTIME}"))
}

unsafe fn driver_store_dir(device_id: u32) -> Result<String, String> {
    let cfg = load_system("cfgmgr32.dll")?;
    let reg = load_system("advapi32.dll")?;
    macro_rules! f {
        ($lib:expr, $name:literal) => {
            *$lib
                .get(concat!($name, "\0").as_bytes())
                .map_err(|e| format!("{}: {e}", $name))?
        };
    }
    let list_size: GetListSize = f!(cfg, "CM_Get_Device_ID_List_SizeW");
    let get_list: GetList = f!(cfg, "CM_Get_Device_ID_ListW");
    let locate: LocateDevNode = f!(cfg, "CM_Locate_DevNodeW");
    let open_key: OpenDevNodeKey = f!(cfg, "CM_Open_DevNode_Key");
    let query: RegQueryValueEx = f!(reg, "RegQueryValueExW");
    let close: RegCloseKey = f!(reg, "RegCloseKey");
    let class = wide(DISPLAY_CLASS);
    let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
    let mut ids: Vec<u16> = Vec::new();
    let mut r = CR_BUFFER_SMALL;
    for _ in 0..4 {
        let mut n = 0u32;
        r = list_size(&mut n, class.as_ptr(), flags);
        if r != CR_SUCCESS {
            break;
        }
        ids = vec![0; n as usize + 1];
        r = get_list(class.as_ptr(), ids.as_mut_ptr(), n, flags);
        if r != CR_BUFFER_SMALL {
            break;
        }
    }
    if r != CR_SUCCESS {
        return Err(format!(
            "CM_Get_Device_ID_ListW failed ({r:#x}) listing display devices"
        ));
    }
    let wanted = format!("VEN_8086&DEV_{device_id:04X}");
    let mut seen = false;
    for id in ids.split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let text = String::from_utf16_lossy(id);
        if intel_device(&text) != Some(device_id) {
            continue;
        }
        seen = true;
        let mut node = 0u32;
        let id_z: Vec<u16> = id.iter().copied().chain(std::iter::once(0)).collect();
        if locate(&mut node, id_z.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
            continue;
        }
        let mut key: Hkey = std::ptr::null_mut();
        let r = open_key(
            node,
            KEY_READ,
            0,
            REG_DISPOSITION_OPEN_EXISTING,
            &mut key,
            CM_REGISTRY_SOFTWARE,
        );
        if r != CR_SUCCESS || key.is_null() {
            continue;
        }
        let mut buf = vec![0u16; MAX_PATH_UNITS];
        let mut bytes = (buf.len() * 2 - 2) as u32;
        let mut kind = 0u32;
        let name = wide(VALUE);
        let q = query(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            buf.as_mut_ptr() as *mut u8,
            &mut bytes,
        );
        close(key);
        if q != 0 || kind != REG_SZ {
            continue;
        }
        let units = (bytes as usize / 2).min(buf.len());
        let n = buf[..units].iter().position(|&c| c == 0).unwrap_or(units);
        let dir = String::from_utf16_lossy(&buf[..n]);
        if !dir.is_empty() {
            return Ok(dir);
        }
    }
    Err(if seen {
        format!("the {wanted} driver names no {VALUE} (a driver without a oneVPL runtime)")
    } else {
        format!("no present display device {wanted}")
    })
}
