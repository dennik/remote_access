//! Screen capture: DXGI Desktop Duplication, with GDI as the fallback for machines
//! or moments where duplication isn't available.

use anyhow::{anyhow, bail, Result};
use rd_common::proto::{CursorShape, MonitorInfo};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits,
    GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HDC,
    HMONITOR, MONITORINFOEXW, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM,
    IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, MONITORINFOF_PRIMARY,
};

pub enum Grab {
    /// `Capturer::frame` holds new pixels.
    Frame,
    /// Nothing changed within the timeout.
    Unchanged,
}

pub trait Capturer {
    fn grab(&mut self, timeout_ms: u32) -> Result<Grab>;
    /// The latest frame as tightly packed BGRA, top row first.
    fn frame(&self) -> (&[u8], u32, u32);
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn callback(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<MonitorInfo>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            list.push(MonitorInfo {
                name: String::from_utf16_lossy(&info.szDevice[..len]),
                x: r.left,
                y: r.top,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        true.into()
    }
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(callback), LPARAM(&mut list as *mut _ as isize));
    }
    // Stable order, with the primary display first.
    list.sort_by_key(|m| (!m.primary, m.x, m.y));
    list
}

pub fn create_capturer(monitor: &MonitorInfo) -> Box<dyn Capturer> {
    match Dxgi::new(monitor) {
        Ok(capturer) => Box::new(capturer),
        Err(_) => Box::new(Gdi::new(monitor.clone())),
    }
}

pub fn current_cursor() -> CursorShape {
    unsafe {
        let mut info = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut info).is_err() {
            return CursorShape::Arrow;
        }
        if info.flags.0 & CURSOR_SHOWING.0 == 0 {
            return CursorShape::Hidden;
        }
        let table = [
            (IDC_ARROW, CursorShape::Arrow),
            (IDC_IBEAM, CursorShape::IBeam),
            (IDC_HAND, CursorShape::Hand),
            (IDC_WAIT, CursorShape::Wait),
            (IDC_APPSTARTING, CursorShape::AppStarting),
            (IDC_CROSS, CursorShape::Cross),
            (IDC_SIZEALL, CursorShape::SizeAll),
            (IDC_SIZENS, CursorShape::SizeNs),
            (IDC_SIZEWE, CursorShape::SizeWe),
            (IDC_SIZENESW, CursorShape::SizeNesw),
            (IDC_SIZENWSE, CursorShape::SizeNwse),
            (IDC_NO, CursorShape::No),
        ];
        for (id, shape) in table {
            if LoadCursorW(None, id).is_ok_and(|c| c == info.hCursor) {
                return shape;
            }
        }
        CursorShape::Arrow
    }
}

// ---------------------------------------------------------------- DXGI

struct Dxgi {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    has_frame: bool,
    fallback: Gdi,
}

impl Dxgi {
    fn new(monitor: &MonitorInfo) -> Result<Self> {
        unsafe {
            let (adapter, output) = find_output(&monitor.name)?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
            let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;
            let duplication = output.DuplicateOutput(&device)?;
            let desc = duplication.GetDesc();
            let (width, height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);

            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut staging = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;

            Ok(Self {
                device,
                context,
                duplication,
                staging,
                buffer: vec![0; (width * height * 4) as usize],
                width,
                height,
                has_frame: false,
                fallback: Gdi::new(monitor.clone()),
            })
        }
    }

    fn copy_out(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            self.context.CopyResource(&self.staging, texture);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            let row = self.width as usize * 4;
            for y in 0..self.height as usize {
                let src = std::slice::from_raw_parts((map.pData as *const u8).add(y * map.RowPitch as usize), row);
                self.buffer[y * row..(y + 1) * row].copy_from_slice(src);
            }
            self.context.Unmap(&self.staging, 0);
        }
        Ok(())
    }
}

impl Capturer for Dxgi {
    fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        let _ = &self.device;
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    // Duplication only reports changes, so a static screen would
                    // otherwise never produce a first picture.
                    if !self.has_frame {
                        self.fallback.grab(0)?;
                        let (pixels, w, h) = self.fallback.frame();
                        if w == self.width && h == self.height {
                            self.buffer.copy_from_slice(pixels);
                            self.has_frame = true;
                            return Ok(Grab::Frame);
                        }
                    }
                    return Ok(Grab::Unchanged);
                }
                Err(e) => return Err(e.into()),
            }

            // Pointer-only updates carry no new desktop image.
            let result = if info.LastPresentTime == 0 {
                Ok(Grab::Unchanged)
            } else {
                match resource.ok_or_else(|| anyhow!("no frame resource")).and_then(|r| Ok(r.cast::<ID3D11Texture2D>()?)) {
                    Ok(texture) => self.copy_out(&texture).map(|_| {
                        self.has_frame = true;
                        Grab::Frame
                    }),
                    Err(e) => Err(e),
                }
            };
            let _ = self.duplication.ReleaseFrame();
            result
        }
    }

    fn frame(&self) -> (&[u8], u32, u32) {
        (&self.buffer, self.width, self.height)
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;
    use std::time::Instant;

    /// Needs a real display, so run explicitly: cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn duplication_works_when_dpi_aware_and_reports_timings() {
        crate::service::set_dpi_aware();
        for monitor in list_monitors() {
            let mut capturer = Dxgi::new(&monitor).expect("desktop duplication should be available");
            capturer.grab(500).expect("first grab");
            let (pixels, w, h) = capturer.frame();
            assert_eq!(pixels.len(), (w * h * 4) as usize);
            assert!(pixels.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]), "captured an all-black frame");

            let start = Instant::now();
            let mut jpeg = Vec::new();
            let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, 75);
            encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
            encoder.encode(pixels, w as u16, h as u16, jpeg_encoder::ColorType::Bgra).unwrap();
            println!("{} {w}x{h}: full frame {} KB, encoded in {:?} on one thread", monitor.name, jpeg.len() / 1024, start.elapsed());
        }
    }
}

fn find_output(device_name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let desc = output.GetDesc()?;
                let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
                if String::from_utf16_lossy(&desc.DeviceName[..len]) == device_name {
                    return Ok((adapter, output.cast()?));
                }
                o += 1;
            }
            a += 1;
        }
    }
    bail!("no DXGI output named {device_name}")
}

// ---------------------------------------------------------------- GDI

struct Gdi {
    monitor: MonitorInfo,
    buffer: Vec<u8>,
}

impl Gdi {
    fn new(monitor: MonitorInfo) -> Self {
        let size = monitor.width as usize * monitor.height as usize * 4;
        Self { monitor, buffer: vec![0; size] }
    }
}

impl Capturer for Gdi {
    fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        // GDI has no change notification, so this is paced by the caller's timeout.
        if timeout_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms.min(33) as u64));
        }
        let (w, h) = (self.monitor.width as i32, self.monitor.height as i32);
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                bail!("GetDC failed");
            }
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(dc, bitmap.into());
            let blit = BitBlt(dc, 0, 0, w, h, Some(screen), self.monitor.x, self.monitor.y, SRCCOPY | CAPTUREBLT);

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative means top row first
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let lines = GetDIBits(
                dc,
                bitmap,
                0,
                h as u32,
                Some(self.buffer.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            );

            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);

            blit?;
            if lines == 0 {
                bail!("GetDIBits failed");
            }
        }
        Ok(Grab::Frame)
    }

    fn frame(&self) -> (&[u8], u32, u32) {
        (&self.buffer, self.monitor.width, self.monitor.height)
    }
}
