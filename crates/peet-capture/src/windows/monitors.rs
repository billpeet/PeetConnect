use peet_proto::{MonitorId, Rect};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
    DXGI_MODE_ROTATION_ROTATE90,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use super::device::dxgi_factory;
use crate::{CaptureError, MonitorDesc, Rotation};

/// `MONITORINFOF_PRIMARY` from WinUser.h; not exported by the `windows` crate.
const MONITORINFOF_PRIMARY: u32 = 1;

/// Lists every monitor attached to the desktop, across all hardware adapters.
///
/// Monitor IDs encode the adapter and output index so they stay stable while the topology
/// does not change.
pub fn enumerate_monitors() -> Result<Vec<MonitorDesc>, CaptureError> {
    let factory = dxgi_factory()?;
    let mut monitors = Vec::new();
    for adapter_index in 0u32.. {
        // SAFETY: plain COM enumeration; NOT_FOUND terminates.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => return Err(err.into()),
        };
        // SAFETY: GetDesc1 fills a plain struct.
        let adapter_desc = unsafe { adapter.GetDesc1() }?;
        if adapter_desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0.cast_unsigned() != 0 {
            continue;
        }
        let adapter_name = utf16_z(&adapter_desc.Description);
        for output_index in 0u32.. {
            // SAFETY: as above.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(err) => return Err(err.into()),
            };
            // SAFETY: GetDesc fills a plain struct.
            let desc: DXGI_OUTPUT_DESC = unsafe { output.GetDesc() }?;
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            monitors.push(describe(&desc, &adapter_name, adapter_index, output_index));
        }
    }
    Ok(monitors)
}

fn utf16_z(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn describe(
    desc: &DXGI_OUTPUT_DESC,
    adapter_name: &str,
    adapter_index: u32,
    output_index: u32,
) -> MonitorDesc {
    let name = utf16_z(&desc.DeviceName);
    let rc = desc.DesktopCoordinates;
    let bounds = Rect {
        x: rc.left,
        y: rc.top,
        width: (rc.right - rc.left).max(0).cast_unsigned(),
        height: (rc.bottom - rc.top).max(0).cast_unsigned(),
    };

    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // SAFETY: valid HMONITOR from DXGI and valid out-pointers.
    let _ = unsafe { GetDpiForMonitor(desc.Monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let scale_percent = u16::try_from(dpi_x * 100 / 96).unwrap_or(100);

    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: info.cbSize is set and the pointer is valid.
    let primary = unsafe { GetMonitorInfoW(desc.Monitor, &mut info) }.as_bool()
        && info.dwFlags & MONITORINFOF_PRIMARY != 0;

    let rotation = match desc.Rotation {
        r if r == DXGI_MODE_ROTATION_ROTATE90 => Rotation::Rotate90,
        r if r == DXGI_MODE_ROTATION_ROTATE180 => Rotation::Rotate180,
        r if r == DXGI_MODE_ROTATION_ROTATE270 => Rotation::Rotate270,
        r if r == DXGI_MODE_ROTATION_IDENTITY => Rotation::Identity,
        _ => Rotation::Identity,
    };

    MonitorDesc {
        id: MonitorId((adapter_index << 8) | output_index),
        name,
        bounds,
        scale_percent,
        primary,
        rotation,
        adapter_name: adapter_name.to_owned(),
        adapter_index,
        output_index,
    }
}
