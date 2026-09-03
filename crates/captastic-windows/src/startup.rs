use std::ffi::c_void;
use std::path::Path;

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{w, Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("Captastic");

pub fn enable_startup(executable: &Path) -> Result<(), CaptureError> {
    if !executable.is_file() {
        return Err(startup_error(
            "enable_startup",
            format!(
                "desktop launcher does not exist at {}",
                executable.display()
            ),
        ));
    }
    let value = startup_value(executable);
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes: Vec<u8> = wide.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    let mut key = Default::default();
    // SAFETY: All names are null-terminated, key receives the opened handle, and no custom security
    // descriptor or class string is requested.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    }
    .map_err(|error| native_error("open_startup_key", error))?;
    // SAFETY: key is live and bytes contains the complete null-terminated UTF-16 REG_SZ payload.
    let result = unsafe { RegSetValueExW(key, VALUE_NAME, 0, REG_SZ, Some(&bytes)) }
        .map_err(|error| native_error("write_startup_value", error));
    // SAFETY: key is no longer used and is closed exactly once.
    let _ = unsafe { RegCloseKey(key) };
    result
}

pub fn disable_startup() -> Result<bool, CaptureError> {
    if startup_command()?.is_none() {
        return Ok(false);
    }
    let mut key = Default::default();
    // SAFETY: All names are null-terminated and key receives a handle with only value-write access.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    }
    .map_err(|error| native_error("open_startup_key", error))?;
    // SAFETY: key is live and VALUE_NAME is a static null-terminated value name.
    let result = unsafe { RegDeleteValueW(key, VALUE_NAME) }
        .map(|()| true)
        .map_err(|error| native_error("delete_startup_value", error));
    // SAFETY: key is no longer used and is closed exactly once.
    let _ = unsafe { RegCloseKey(key) };
    result
}

pub fn startup_command() -> Result<Option<String>, CaptureError> {
    let mut byte_length = 0_u32;
    // SAFETY: This sizing call writes only the required byte count and returns no value data.
    let size_result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut byte_length),
        )
    };
    if let Err(error) = size_result {
        if is_not_found(&error) {
            return Ok(None);
        }
        return Err(native_error("read_startup_value_size", error));
    }
    if byte_length == 0 {
        return Ok(None);
    }
    let mut value = vec![0_u16; (byte_length as usize).div_ceil(2)];
    // SAFETY: value has byte_length bytes of writable storage and the registry call is constrained
    // to REG_SZ data.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(value.as_mut_ptr().cast::<c_void>()),
            Some(&mut byte_length),
        )
    }
    .map_err(|error| native_error("read_startup_value", error))?;
    Ok(Some(decode_registry_string(&value)))
}

fn startup_value(executable: &Path) -> String {
    format!("\"{}\"", executable.display())
}

fn decode_registry_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn is_not_found(error: &WindowsError) -> bool {
    error.code().0 == hresult_from_win32(ERROR_FILE_NOT_FOUND.0)
        || error.code().0 == hresult_from_win32(ERROR_PATH_NOT_FOUND.0)
}

const fn hresult_from_win32(code: u32) -> i32 {
    ((code & 0xffff) | 0x8007_0000) as i32
}

fn native_error(operation: &'static str, error: WindowsError) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-startup",
        operation,
        message: error.to_string(),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
    }
}

fn startup_error(operation: &'static str, message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::SourceUnavailable,
        backend: "windows-startup",
        operation,
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_paths_with_spaces() {
        assert_eq!(
            startup_value(Path::new(
                r"C:\Program Files\Captastic\captastic-desktop.exe"
            )),
            r#""C:\Program Files\Captastic\captastic-desktop.exe""#
        );
    }

    #[test]
    fn registry_strings_stop_at_the_first_null() {
        assert_eq!(decode_registry_string(&[67, 97, 112, 0, 88]), "Cap");
    }
}
