//! Thin, safe-ish wrappers over the IOKit calls pipecap needs.
//!
//! Two kinds of information are read from the I/O Registry:
//!
//! * `DCPAVServiceProxy` services, one per display output, through which the
//!   private `IOAVService*` API reads the EDID and installs a *virtual* EDID.
//! * `AppleDisplayConnectionManager`, the display crossbar that assigns SoC
//!   display pipes (`dispextN`) to physical ports. Its `ConnectionMapping`
//!   property tells how many pipes each monitor needs and got.

use anyhow::{anyhow, bail, Result};
use core_foundation::base::{CFGetTypeID, CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation::{array::CFArray, boolean::CFBoolean};
use core_foundation_sys::base::{kCFAllocatorDefault, CFAllocatorRef, CFTypeRef};
use core_foundation_sys::data::CFDataRef;
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use core_foundation_sys::string::CFStringRef;
use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

type IoObject = u32;
type KernReturn = i32;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: CFDictionaryRef,
        existing: *mut IoObject,
    ) -> KernReturn;
    fn IOIteratorNext(iterator: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
    fn IORegistryEntryCreateCFProperties(
        entry: IoObject,
        properties: *mut CFMutableDictionaryRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> KernReturn;
    fn IORegistryEntryGetPath(
        entry: IoObject,
        plane: *const c_char,
        path: *mut c_char,
    ) -> KernReturn;
    fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> KernReturn;

    // Private API (also used by BetterDisplay, m1ddc and friends).
    fn IOAVServiceCreateWithService(allocator: CFAllocatorRef, service: IoObject) -> CFTypeRef;
    fn IOAVServiceCopyEDID(service: CFTypeRef, edid: *mut CFDataRef) -> KernReturn;
    fn IOAVServiceSetVirtualEDIDMode(service: CFTypeRef, mode: u32, edid: CFDataRef) -> KernReturn;
}

/// Translate an `IOReturn` into something a human can act on.
pub fn ioreturn_message(r: i32) -> String {
    let code = r as u32;
    let name = match code {
        0xe00002bc => "kIOReturnError (general error)",
        0xe00002bd => "kIOReturnNoMemory",
        0xe00002c1 => "kIOReturnNotPrivileged",
        0xe00002c2 => "kIOReturnBadArgument (the EDID was rejected)",
        0xe00002c7 => "kIOReturnUnsupported",
        0xe00002cd => "kIOReturnNotOpen",
        0xe00002d8 => "kIOReturnNotAttached",
        0xe00002e2 => {
            "kIOReturnNotPermitted: the IOKit user client could not be opened. \
             Run pipecap directly from Terminal, outside of any sandbox"
        }
        0xe00002ed => "kIOReturnNotReady",
        0xffffffff => {
            "IOAVServiceCreateWithService returned NULL (sandboxed process? run from Terminal)"
        }
        _ => "unknown IOReturn",
    };
    format!("{name} (0x{code:08x})")
}

/// An `io_service_t` handle, released on drop.
pub struct Service(IoObject);

impl Drop for Service {
    fn drop(&mut self) {
        unsafe {
            IOObjectRelease(self.0);
        }
    }
}

impl Service {
    /// All properties of the registry entry as JSON (CFData becomes hex).
    pub fn properties(&self) -> Value {
        unsafe {
            let mut dict: CFMutableDictionaryRef = ptr::null_mut();
            let kr = IORegistryEntryCreateCFProperties(self.0, &mut dict, kCFAllocatorDefault, 0);
            if kr != 0 || dict.is_null() {
                return Value::Null;
            }
            let d = CFDictionary::<CFType, CFType>::wrap_under_create_rule(dict as CFDictionaryRef);
            cf_to_json(d.as_CFTypeRef())
        }
    }

    pub fn path(&self) -> String {
        let mut buf = [0 as c_char; 512];
        unsafe {
            if IORegistryEntryGetPath(self.0, c"IOService".as_ptr(), buf.as_mut_ptr()) != 0 {
                return String::new();
            }
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    }

    pub fn name(&self) -> String {
        let mut buf = [0 as c_char; 128];
        unsafe {
            if IORegistryEntryGetName(self.0, buf.as_mut_ptr()) != 0 {
                return String::new();
            }
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    }
}

/// Every registry entry whose class is (or inherits from) `class`.
pub fn services_of_class(class: &str) -> Result<Vec<Service>> {
    let c = CString::new(class)?;
    unsafe {
        let matching = IOServiceMatching(c.as_ptr());
        if matching.is_null() {
            bail!("IOServiceMatching({class}) returned NULL");
        }
        let mut it: IoObject = 0;
        // IOServiceGetMatchingServices consumes one reference of `matching`.
        let kr = IOServiceGetMatchingServices(0, matching as CFDictionaryRef, &mut it);
        if kr != 0 {
            bail!(
                "IOServiceGetMatchingServices({class}) failed: {}",
                ioreturn_message(kr)
            );
        }
        let mut out = Vec::new();
        loop {
            let s = IOIteratorNext(it);
            if s == 0 {
                break;
            }
            out.push(Service(s));
        }
        IOObjectRelease(it);
        Ok(out)
    }
}

/// Convert any Core Foundation object into a `serde_json::Value`.
///
/// # Safety
/// `v` must be a valid CF object (or NULL). The caller keeps ownership.
unsafe fn cf_to_json(v: CFTypeRef) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let tid = CFGetTypeID(v);
    if tid == CFString::type_id() {
        Value::String(CFString::wrap_under_get_rule(v as CFStringRef).to_string())
    } else if tid == CFNumber::type_id() {
        let n = CFNumber::wrap_under_get_rule(v as _);
        match (n.to_i64(), n.to_f64()) {
            (Some(i), _) => json!(i),
            (None, Some(f)) => json!(f),
            _ => Value::Null,
        }
    } else if tid == CFBoolean::type_id() {
        Value::Bool(CFBoolean::wrap_under_get_rule(v as _).into())
    } else if tid == CFData::type_id() {
        let d = CFData::wrap_under_get_rule(v as CFDataRef);
        Value::String(crate::edid::hex(d.bytes()))
    } else if tid == CFArray::<CFType>::type_id() {
        let a = CFArray::<CFType>::wrap_under_get_rule(v as _);
        Value::Array(a.iter().map(|x| cf_to_json(x.as_CFTypeRef())).collect())
    } else if tid == CFDictionary::<CFType, CFType>::type_id() {
        let d = CFDictionary::<CFType, CFType>::wrap_under_get_rule(v as _);
        let (keys, vals) = d.get_keys_and_values();
        let mut map = serde_json::Map::new();
        for (k, val) in keys.into_iter().zip(vals) {
            let key = if CFGetTypeID(k as CFTypeRef) == CFString::type_id() {
                CFString::wrap_under_get_rule(k as CFStringRef).to_string()
            } else {
                format!("{k:?}")
            };
            map.insert(key, cf_to_json(val as CFTypeRef));
        }
        Value::Object(map)
    } else {
        Value::String(format!("<CFTypeID {tid}>"))
    }
}

/// One external display output (a `DCPAVServiceProxy` with `Location != Embedded`).
#[derive(Serialize)]
pub struct Display {
    pub index: usize,
    pub path: String,
    /// The `dispextN` pipe node this output is attached to, if visible in the path.
    pub pipe: Option<String>,
    pub location: String,
    #[serde(serialize_with = "ser_opt_hex")]
    pub edid: Option<Vec<u8>>,
    pub copy_edid_status: i32,
    #[serde(skip)]
    service: Service,
}

fn ser_opt_hex<S: serde::Serializer>(b: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
    match b {
        Some(b) => s.serialize_str(&crate::edid::hex(b)),
        None => s.serialize_none(),
    }
}

impl Display {
    fn av_service(&self) -> Result<CFType> {
        unsafe {
            let av = IOAVServiceCreateWithService(kCFAllocatorDefault, self.service.0);
            if av.is_null() {
                bail!("IOAVServiceCreateWithService failed for {}", self.path);
            }
            Ok(CFType::wrap_under_create_rule(av))
        }
    }

    /// Install (`Some`) or remove (`None`) a virtual EDID on this output.
    pub fn set_virtual_edid(&self, edid: Option<&[u8]>) -> Result<()> {
        let av = self.av_service()?;
        let r = unsafe {
            match edid {
                Some(b) => {
                    let d = CFData::from_buffer(b);
                    IOAVServiceSetVirtualEDIDMode(av.as_CFTypeRef(), 1, d.as_concrete_TypeRef())
                }
                None => IOAVServiceSetVirtualEDIDMode(av.as_CFTypeRef(), 0, ptr::null()),
            }
        };
        if r != 0 {
            return Err(anyhow!(
                "IOAVServiceSetVirtualEDIDMode failed: {}",
                ioreturn_message(r)
            ));
        }
        Ok(())
    }

    pub fn edid_id(&self) -> Option<String> {
        self.edid.as_deref().and_then(crate::edid::id_of)
    }
}

/// Read the EDID of a `DCPAVServiceProxy`. Returns the bytes and the IOReturn.
///
/// # Safety
/// `service` must be a live `io_service_t`.
unsafe fn read_edid(service: IoObject) -> (Option<Vec<u8>>, i32) {
    let av = IOAVServiceCreateWithService(kCFAllocatorDefault, service);
    if av.is_null() {
        return (None, -1);
    }
    let av = CFType::wrap_under_create_rule(av);
    let mut data: CFDataRef = ptr::null();
    let status = IOAVServiceCopyEDID(av.as_CFTypeRef(), &mut data);
    if status == 0 && !data.is_null() {
        let d = CFData::wrap_under_create_rule(data);
        (Some(d.bytes().to_vec()), status)
    } else {
        (None, status)
    }
}

/// Enumerate external display outputs and read their EDIDs.
pub fn external_displays() -> Result<Vec<Display>> {
    let svcs = services_of_class("DCPAVServiceProxy")?;
    if svcs.is_empty() {
        bail!("no DCPAVServiceProxy services found; pipecap needs an Apple Silicon Mac");
    }
    let mut out = Vec::new();
    for service in svcs {
        let props = service.properties();
        let location = props["Location"].as_str().unwrap_or("").to_string();
        if location == "Embedded" {
            continue;
        }
        let path = service.path();
        let pipe = path
            .split('/')
            .filter_map(|seg| seg.split(':').next())
            .find(|s| s.starts_with("dispext") || s.starts_with("disp"))
            .map(str::to_string);
        let (edid, status) = unsafe { read_edid(service.0) };
        let index = out.len();
        out.push(Display {
            index,
            path,
            pipe,
            location,
            edid,
            copy_edid_status: status,
            service,
        });
    }
    Ok(out)
}

/// One entry of the crossbar's `ConnectionMapping`.
#[derive(Debug, Clone, Serialize)]
pub struct PipeMapping {
    pub product_name: String,
    pub product_id: i64,
    pub address: String,
    pub max_w: i64,
    pub max_h: i64,
    pub max_pipes: i64,
    pub pipe_ids: Vec<i64>,
    pub max_active_pixel_rate: i64,
    pub max_total_pixel_rate: i64,
    pub max_bpc: i64,
}

impl PipeMapping {
    /// Whether this crossbar entry describes the monitor with the given EDID.
    pub fn matches(&self, info: &crate::edid::EdidInfo) -> bool {
        let name_ok = info
            .name
            .as_deref()
            .map(|n| !n.is_empty() && n.trim().eq_ignore_ascii_case(self.product_name.trim()))
            .unwrap_or(false);
        let pid = self.product_id;
        let le = info.product_code as i64;
        let be = info.product_code.swap_bytes() as i64;
        name_ok || (pid != 0 && (pid == le || pid == be))
    }
}

/// State of the display crossbar.
#[derive(Debug, Clone, Serialize)]
pub struct CrossbarState {
    pub mappings: Vec<PipeMapping>,
    pub pending: Vec<String>,
    pub available_ufps: Vec<String>,
    pub current_state: Value,
}

fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Read the crossbar (`AppleDisplayConnectionManager`). `None` when absent.
pub fn crossbar() -> Result<Option<CrossbarState>> {
    let svcs = services_of_class("AppleDisplayConnectionManager")?;
    let Some(s) = svcs.first() else {
        return Ok(None);
    };
    let p = s.properties();
    let mappings = p["ConnectionMapping"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| PipeMapping {
                    product_name: m["ProductName"].as_str().unwrap_or("").trim().to_string(),
                    product_id: m["ProductID"].as_i64().unwrap_or(0),
                    address: m["Address"].as_str().unwrap_or("").to_string(),
                    max_w: m["MaxW"].as_i64().unwrap_or(0),
                    max_h: m["MaxH"].as_i64().unwrap_or(0),
                    max_pipes: m["MaxPipes"].as_i64().unwrap_or(0),
                    pipe_ids: m["PipeIDs"]
                        .as_array()
                        .map(|x| x.iter().filter_map(Value::as_i64).collect())
                        .unwrap_or_default(),
                    max_active_pixel_rate: m["MaxActivePixelRate"].as_i64().unwrap_or(0),
                    max_total_pixel_rate: m["MaxTotalPixelRate"].as_i64().unwrap_or(0),
                    max_bpc: m["MaxBpc"].as_i64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(CrossbarState {
        mappings,
        pending: str_array(&p["pending-dfps"]),
        available_ufps: str_array(&p["available-ufps"]),
        current_state: p["current-state"].clone(),
    }))
}

/// Number of external display pipe nodes (`dispextN`) in the device tree.
pub fn pipe_count() -> usize {
    services_of_class("AppleARMIODevice")
        .map(|v| v.iter().filter(|s| s.name().starts_with("dispext")).count())
        .unwrap_or(0)
}

/// Per-pipe pixel-rate limits published by the DisplayPort TX ports.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PipeLimits {
    pub max_active_pixel_rate: i64,
    pub max_total_pixel_rate: i64,
}

impl PipeLimits {
    /// Values observed on a t6050-class SoC; used when the registry has none.
    pub const FALLBACK: PipeLimits = PipeLimits {
        max_active_pixel_rate: 1_274_019_840,
        max_total_pixel_rate: 1_438_000_000,
    };
}

pub fn pipe_limits() -> Option<PipeLimits> {
    let svcs = services_of_class("AFKEPInterfaceKextV2").ok()?;
    let mut best: Option<PipeLimits> = None;
    for s in svcs {
        let p = s.properties();
        if let (Some(a), Some(t)) = (
            p["MaxActivePixelRate"].as_i64(),
            p["MaxTotalPixelRate"].as_i64(),
        ) {
            if best.map(|b| a > b.max_active_pixel_rate).unwrap_or(true) {
                best = Some(PipeLimits {
                    max_active_pixel_rate: a,
                    max_total_pixel_rate: t,
                });
            }
        }
    }
    best
}

/// A display that currently has a framebuffer (i.e. is really lit up).
#[derive(Debug, Clone, Serialize)]
pub struct Framebuffer {
    pub product_name: String,
    pub max_refresh_hz: i64,
    pub port_id: i64,
    pub pipe: String,
}

pub fn active_framebuffers() -> Vec<Framebuffer> {
    let Ok(svcs) = services_of_class("IOMobileFramebufferShim") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in svcs {
        let p = s.properties();
        let attrs = &p["DisplayAttributes"];
        let Some(name) = attrs["ProductAttributes"]["ProductName"].as_str() else {
            continue;
        };
        let pipe = s
            .path()
            .split('/')
            .find(|seg| seg.starts_with("disp"))
            .map(|seg| seg.split('@').next().unwrap_or(seg).to_string())
            .unwrap_or_default();
        out.push(Framebuffer {
            product_name: name.trim().to_string(),
            max_refresh_hz: attrs["MaximumRefreshRate"].as_i64().unwrap_or(0),
            port_id: attrs["PortID"].as_i64().unwrap_or(0),
            pipe,
        });
    }
    out
}
