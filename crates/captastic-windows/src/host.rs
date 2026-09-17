//! Read-only facts about the machine, for the fingerprint a benchmark report carries.
//!
//! Milestone 5 will not publish a latency figure without "three compatible repeat runs", and the
//! word doing the work there is *compatible*. Two runs are only comparable if they happened on the
//! same machine in the same state, and "the same machine" is not something a build version can
//! answer: the same binary on the same desk produces different numbers on a different GPU driver,
//! at a different refresh rate, on battery, or under Remote Desktop.
//!
//! So this module asks. Every probe here is a question about the host, never an instruction to it:
//! nothing below opens a handle for writing, creates a duplication, or holds a resource past the
//! call. `adapters` builds its own DXGI factory and *enumerates* — it never duplicates an output,
//! because the daemon may well be holding the only duplication this process is allowed and a
//! fingerprint has no business competing with a capture for it.
//!
//! Every probe answers `None`, or an empty list, rather than failing. A fingerprint is evidence
//! about a run, and a run that succeeded must still be describable when one fact about the host was
//! refused — which is the normal case, not the exotic one: CI runs on `windows-latest`, where there
//! is a software adapter and frequently no interactive session at all.

use std::ffi::c_void;

use windows::core::{w, ComInterface, GUID, PCWSTR};
use windows::Win32::Foundation::{LocalFree, HLOCAL, LUID};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE,
    DXGI_ERROR_NOT_FOUND,
};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerGetActiveScheme, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

use crate::dxgi::wide_array_to_string;

/// Where Windows records its own build number, and the only place that carries the UBR.
///
/// `GetVersionEx` lies to unmanifested callers and `RtlGetVersion` omits the update revision
/// entirely, so the registry is the one source that can tell 26100.1742 from 26100.4061 — which is
/// the granularity a driver-level performance change actually arrives at.
const CURRENT_VERSION_KEY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");

/// The first logical processor's node, which is where Windows publishes the CPU's marketing name.
const CENTRAL_PROCESSOR_KEY: PCWSTR = w!("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");

/// One graphics adapter, as the host fingerprint describes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterInfo {
    /// The adapter's marketing name, e.g. `NVIDIA GeForce RTX 3070`.
    pub description: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// The same locally unique id the capture path already keys displays on.
    pub luid: i64,
    /// Whether this is a software rasterizer rather than a GPU.
    ///
    /// The one fact that decides whether a GPU timing budget means anything: hosted CI composes on
    /// a software adapter, where every acquisition figure describes the CPU emulating a GPU.
    pub software: bool,
    pub dedicated_video_memory_mb: u64,
    /// The user-mode display driver's version, e.g. `31.0.15.3699`, when the adapter reports one.
    pub driver_version: Option<String>,
}

/// What the host fingerprint knows about a display that `DisplayInfo` does not carry.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayHardware {
    /// The persistent display id, matching [`captastic_core::DisplayInfo::id`].
    pub display_id: String,
    pub adapter_luid: Option<i64>,
    /// The active refresh rate in Hz, when Windows reports one for the path.
    pub refresh_hz: Option<f64>,
}

/// Where the machine's power is coming from, and which plan is governing it.
///
/// Both change measured latency without changing anything the build or the display geometry can
/// see: a laptop on battery under the balanced plan parks cores and clocks the GPU down, and the
/// resulting numbers are not the ones the same machine produces on mains power.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PowerStatus {
    /// `ac`, `battery`, or `unknown`.
    pub source: Option<String>,
    /// The active power plan's name where it is one of the documented schemes, else its GUID.
    pub plan: Option<String>,
}

/// The Windows build, as `CurrentBuild.UBR (DisplayVersion)` where each part is available.
pub fn os_build() -> Option<String> {
    let mut text = registry_string(CURRENT_VERSION_KEY, w!("CurrentBuild"))?;
    if text.is_empty() {
        return None;
    }
    if let Some(revision) = registry_dword(CURRENT_VERSION_KEY, w!("UBR")) {
        text.push('.');
        text.push_str(&revision.to_string());
    }
    if let Some(version) = registry_string(CURRENT_VERSION_KEY, w!("DisplayVersion"))
        .filter(|version| !version.is_empty())
    {
        text.push_str(&format!(" ({version})"));
    }
    Some(text)
}

/// The CPU's marketing name, as Windows recorded it for the first logical processor.
pub fn processor_name() -> Option<String> {
    registry_string(CENTRAL_PROCESSOR_KEY, w!("ProcessorNameString"))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Every graphics adapter DXGI will enumerate, in enumeration order.
///
/// Enumeration only. No output is duplicated and no device is created, so this is safe to call
/// while the daemon holds a duplication of the same output — which it normally is, and which a
/// probe that competed for the duplication would break.
pub fn adapters() -> Vec<AdapterInfo> {
    // SAFETY: The generic result names a supported DXGI factory interface and Windows initializes
    // the out-parameter; the factory is dropped at the end of this function.
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(factory) => factory,
        Err(error) => {
            log::debug!("host fingerprint could not create a DXGI factory: {error}");
            return Vec::new();
        }
    };
    let mut adapters = Vec::new();
    let mut index = 0_u32;
    loop {
        // SAFETY: index is an enumeration index and NOT_FOUND terminates the walk.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) => {
                if error.code() != DXGI_ERROR_NOT_FOUND {
                    log::debug!(
                        "host fingerprint stopped enumerating adapters at {index}: {error}"
                    );
                }
                break;
            }
        };
        index = index.saturating_add(1);
        let mut desc = DXGI_ADAPTER_DESC1::default();
        // SAFETY: desc is valid writable storage for the live adapter.
        if let Err(error) = unsafe { adapter.GetDesc1(&mut desc) } {
            log::debug!("host fingerprint could not describe adapter {index}: {error}");
            continue;
        }
        // SAFETY: The interface id is a static GUID and the call only reports a version number;
        // it creates nothing. A failure means the adapter cannot host a D3D device, which is
        // reported as an absent driver version rather than as an absent adapter.
        let driver_version = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
            .ok()
            .map(user_mode_driver_version);
        adapters.push(AdapterInfo {
            description: wide_array_to_string(&desc.Description),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            luid: luid_to_i64(desc.AdapterLuid),
            software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 != 0,
            dedicated_video_memory_mb: (desc.DedicatedVideoMemory as u64) / (1024 * 1024),
            driver_version,
        });
    }
    adapters
}

/// Which adapter drives each attached display, and at what refresh rate.
///
/// Empty when the session cannot answer — a locked or disconnected session refuses the display
/// configuration query, and that is a fact about the run rather than a failure of it.
pub fn display_hardware() -> Vec<DisplayHardware> {
    crate::dxgi::display_hardware()
}

/// Where the machine's power comes from and which plan is in force.
pub fn power_status() -> PowerStatus {
    PowerStatus {
        source: power_source(),
        plan: active_power_plan(),
    }
}

fn power_source() -> Option<String> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: status is valid writable storage for the call to fill.
    unsafe { GetSystemPowerStatus(&mut status) }.ok()?;
    Some(
        match status.ACLineStatus {
            0 => "battery",
            1 => "ac",
            // 255 is documented as unknown, and anything else is undocumented; both mean the same
            // thing to a reader deciding whether to trust a number.
            _ => "unknown",
        }
        .to_owned(),
    )
}

/// The documented power schemes, so a fingerprint reads as a plan name rather than as a GUID.
const BALANCED: GUID = GUID::from_u128(0x381b_4222_f694_41f0_9685_ff5b_b260_df2e);
const HIGH_PERFORMANCE: GUID = GUID::from_u128(0x8c5e_7fda_e8bf_4a96_9a85_a6e2_3a8c_635c);
const POWER_SAVER: GUID = GUID::from_u128(0xa184_1308_3541_4fab_bc81_f715_56f2_0b4a);
const ULTIMATE_PERFORMANCE: GUID = GUID::from_u128(0xe9a4_2b02_d5df_448d_aa00_03f1_4749_eb61);

fn active_power_plan() -> Option<String> {
    let mut scheme: *mut GUID = std::ptr::null_mut();
    // SAFETY: scheme is a valid writable pointer slot. A null root key asks for the active scheme
    // of the current user, which is the one governing this run.
    unsafe { PowerGetActiveScheme(HKEY::default(), &mut scheme) }.ok()?;
    if scheme.is_null() {
        return None;
    }
    // SAFETY: The call above succeeded and wrote a pointer to a GUID it allocated, checked
    // non-null immediately above; the value is copied out before the allocation is released.
    let value = unsafe { *scheme };
    // SAFETY: scheme is the block Windows allocated for this call, freed exactly once and never
    // read again afterwards. Windows documents `LocalFree` as the way to release it.
    let _ = unsafe { LocalFree(HLOCAL(scheme.cast::<std::ffi::c_void>())) };
    Some(power_plan_name(&value))
}

fn power_plan_name(scheme: &GUID) -> String {
    match *scheme {
        BALANCED => "balanced".to_owned(),
        HIGH_PERFORMANCE => "high performance".to_owned(),
        POWER_SAVER => "power saver".to_owned(),
        ULTIMATE_PERFORMANCE => "ultimate performance".to_owned(),
        // An OEM or a group policy can define its own scheme. The raw GUID is still evidence: two
        // runs under the same unnamed plan match, and under different ones they do not.
        other => format!("{other:?}"),
    }
}

/// Formats the QWORD `CheckInterfaceSupport` reports as the four-part driver version users see.
fn user_mode_driver_version(raw: i64) -> String {
    let value = raw as u64;
    format!(
        "{}.{}.{}.{}",
        (value >> 48) & 0xffff,
        (value >> 32) & 0xffff,
        (value >> 16) & 0xffff,
        value & 0xffff
    )
}

fn luid_to_i64(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

/// Reads a `REG_SZ` value under `HKEY_LOCAL_MACHINE`, or `None` when it is absent or unreadable.
fn registry_string(key: PCWSTR, value_name: PCWSTR) -> Option<String> {
    let mut byte_length = 0_u32;
    // SAFETY: This sizing call writes only the required byte count and returns no value data.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value_name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut byte_length),
        )
    }
    .ok()?;
    if byte_length == 0 {
        return None;
    }
    let mut data = vec![0_u16; (byte_length as usize).div_ceil(2)];
    // SAFETY: data has byte_length bytes of writable storage and the call is constrained to REG_SZ.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value_name,
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast::<c_void>()),
            Some(&mut byte_length),
        )
    }
    .ok()?;
    Some(wide_array_to_string(&data))
}

/// Reads a `REG_DWORD` value under `HKEY_LOCAL_MACHINE`, or `None` when it is absent.
fn registry_dword(key: PCWSTR, value_name: PCWSTR) -> Option<u32> {
    let mut data = 0_u32;
    let mut byte_length = std::mem::size_of::<u32>() as u32;
    // SAFETY: data is a writable u32, byte_length describes exactly its size, and the call is
    // constrained to REG_DWORD values so nothing larger can be written into it.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value_name,
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::addr_of_mut!(data).cast::<c_void>()),
            Some(&mut byte_length),
        )
    }
    .ok()?;
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_driver_version_reads_as_its_four_parts() {
        // The QWORD is four packed WORDs, most significant first: this is the string a user sees
        // in Device Manager, and a fingerprint that scrambled it would name a driver nobody has.
        assert_eq!(
            user_mode_driver_version(0x001f_0000_000f_0e73),
            "31.0.15.3699"
        );
        assert_eq!(user_mode_driver_version(0), "0.0.0.0");
        // The value arrives as a signed integer; the top bit must not turn into a sign.
        assert_eq!(user_mode_driver_version(-1), "65535.65535.65535.65535");
    }

    #[test]
    fn a_known_scheme_is_named_and_an_unknown_one_is_still_evidence() {
        assert_eq!(power_plan_name(&BALANCED), "balanced");
        assert_eq!(power_plan_name(&HIGH_PERFORMANCE), "high performance");
        assert_eq!(power_plan_name(&POWER_SAVER), "power saver");
        assert_eq!(
            power_plan_name(&ULTIMATE_PERFORMANCE),
            "ultimate performance"
        );
        // An OEM scheme still distinguishes two runs from each other, which is the whole job.
        let custom = GUID::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001);
        assert_ne!(power_plan_name(&custom), "balanced");
        assert!(!power_plan_name(&custom).is_empty());
    }

    /// Runs every real probe. Asserts only what is true of any machine able to run it.
    ///
    /// Including a hosted CI runner with a software adapter and no interactive session, which is
    /// exactly the host these probes must not panic on.
    #[test]
    fn the_live_probes_answer_without_panicking() {
        if let Some(build) = os_build() {
            assert!(!build.is_empty());
        }
        if let Some(cpu) = processor_name() {
            assert!(!cpu.is_empty());
        }
        for adapter in adapters() {
            assert!(!adapter.description.is_empty());
            if let Some(version) = adapter.driver_version.as_deref() {
                assert!(!version.is_empty());
            }
        }
        for display in display_hardware() {
            assert!(!display.display_id.is_empty());
            if let Some(refresh) = display.refresh_hz {
                assert!(refresh > 0.0, "{refresh}");
            }
        }
        let power = power_status();
        if let Some(source) = power.source.as_deref() {
            assert!(["ac", "battery", "unknown"].contains(&source), "{source}");
        }
        if let Some(plan) = power.plan.as_deref() {
            assert!(!plan.is_empty());
        }
    }
}
