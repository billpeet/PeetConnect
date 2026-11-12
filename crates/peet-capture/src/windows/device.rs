use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_FLAG, D3D11_CPU_ACCESS_FLAG, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};

use crate::CaptureError;

/// A Direct3D 11 device bound to one graphics adapter.
///
/// Capture, colour conversion and hardware encoding all run on this device so frames never
/// leave the GPU. The immediate context is multithread-protected because Media Foundation
/// encoders use it from their own threads.
#[derive(Debug, Clone)]
pub struct D3dDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter1,
    pub adapter_index: u32,
}

impl D3dDevice {
    /// Creates a device on the adapter with the given DXGI index.
    pub fn for_adapter(adapter_index: u32) -> Result<Self, CaptureError> {
        let factory = dxgi_factory()?;
        // SAFETY: plain COM call; an out-of-range index yields DXGI_ERROR_NOT_FOUND.
        let adapter = unsafe { factory.EnumAdapters1(adapter_index) }?;
        Self::on_adapter(adapter, adapter_index)
    }

    pub(crate) fn on_adapter(
        adapter: IDXGIAdapter1,
        adapter_index: u32,
    ) -> Result<Self, CaptureError> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        // SAFETY: out-pointers are valid for the duration of the call.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| CaptureError::Unsupported("no D3D11 device".into()))?;
        let context =
            context.ok_or_else(|| CaptureError::Unsupported("no D3D11 context".into()))?;

        // SAFETY: ID3D11Multithread is implemented by every D3D11 immediate context.
        unsafe {
            if let Ok(mt) = context.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        tracing::debug!(adapter_index, ?level, "created D3D11 device");
        Ok(Self {
            device,
            context,
            adapter,
            adapter_index,
        })
    }

    /// Creates a 2D texture with no mips and a single sample.
    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        usage: D3D11_USAGE,
        bind: D3D11_BIND_FLAG,
        cpu_access: D3D11_CPU_ACCESS_FLAG,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: usage,
            BindFlags: bind.0.cast_unsigned(),
            CPUAccessFlags: cpu_access.0.cast_unsigned(),
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: desc outlives the call; texture is a valid out-pointer.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        texture.ok_or_else(|| CaptureError::Unsupported("CreateTexture2D returned null".into()))
    }
}

pub(crate) fn dxgi_factory() -> Result<IDXGIFactory1, CaptureError> {
    // SAFETY: plain COM factory creation.
    Ok(unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }?)
}
