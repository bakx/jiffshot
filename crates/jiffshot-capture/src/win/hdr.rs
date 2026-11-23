//! HDR monitor snapshots in linear scRGB, converted to the app's SDR frames.

use crate::{from_wide, Frame, Rect};
use half::f16;
use std::io;
use std::mem::size_of;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{Error, Interface, HSTRING};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, E_FAIL, E_NOINTERFACE, E_NOTIMPL, REGDB_E_CLASSNOTREG,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{
    RoGetActivationFactory, RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
};

struct Display {
    handle: isize,
    bounds: Rect,
    area: Rect,
    white_scale: f32,
}

fn displays(region: Rect) -> Vec<Display> {
    // SAFETY: DXGI owns the returned interfaces and monitor handles.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let adapters = (0..).map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok());
    let outputs = adapters
        .flat_map(|adapter| (0..).map_while(move |i| unsafe { adapter.EnumOutputs(i) }.ok()));
    let mut white_levels = None;
    outputs
        .filter_map(|output| {
            let desc = unsafe { output.cast::<IDXGIOutput6>().ok()?.GetDesc1() }.ok()?;
            if !desc.AttachedToDesktop.as_bool()
                || desc.Monitor.0.is_null()
                || desc.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
            {
                return None;
            }
            let r = desc.DesktopCoordinates;
            let bounds = Rect::from_ltrb(r.left, r.top, r.right, r.bottom);
            let area = region.intersect(bounds)?;
            let name = from_wide(&desc.DeviceName);
            let levels = white_levels.get_or_insert_with(sdr_white_levels);
            let white_scale = levels
                .iter()
                .find(|(device, _)| device.eq_ignore_ascii_case(&name))
                .map_or(1.0, |(_, scale)| *scale);
            Some(Display {
                handle: desc.Monitor.0 as isize,
                bounds,
                area,
                white_scale,
            })
        })
        .collect()
}

fn sdr_white_levels() -> Vec<(String, f32)> {
    // Retry if a display is connected or disconnected between the two calls.
    for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0, 0);
        // SAFETY: both counts and the arrays below are valid writable buffers.
        if unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        } != ERROR_SUCCESS
        {
            break;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        match unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        } {
            ERROR_SUCCESS => {
                paths.truncate(path_count as usize);
                return paths.iter().filter_map(sdr_white_level).collect();
            }
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => break,
        }
    }
    Vec::new()
}

fn sdr_white_level(path: &DISPLAYCONFIG_PATH_INFO) -> Option<(String, f32)> {
    let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        SDRWhiteLevel: 0,
    };
    let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the headers are the first fields of their correctly sized packets.
    unsafe {
        if DisplayConfigGetDeviceInfo(std::ptr::from_mut(&mut white).cast()) != 0
            || white.SDRWhiteLevel == 0
            || DisplayConfigGetDeviceInfo(std::ptr::from_mut(&mut source).cast()) != 0
        {
            return None;
        }
    }
    // Windows reports the SDR white level in thousandths of the 80-nit scRGB reference.
    Some((
        from_wide(&source.viewGdiDeviceName),
        white.SDRWhiteLevel as f32 / 1000.0,
    ))
}

pub(super) fn grab(region: Rect) -> io::Result<Option<Frame>> {
    let displays = displays(region);
    if displays.is_empty() {
        return Ok(None);
    }
    // Use an MTA worker even when the caller is the UI's STA thread.
    std::thread::Builder::new()
        .name("jiffshot-hdr-capture".into())
        .spawn(move || capture(displays, region))?
        .join()
        .map_err(|_| io::Error::other("HDR screenshot worker panicked"))?
}

fn capture(displays: Vec<Display>, region: Rect) -> io::Result<Option<Frame>> {
    let _apartment = Apartment::initialize().map_err(capture_error)?;
    let start = || -> windows::core::Result<_> {
        let device = create_device()?;
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: wraps our valid Direct3D device for Windows Graphics Capture.
        let winrt: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let interop: IGraphicsCaptureItemInterop = unsafe {
            RoGetActivationFactory(&HSTRING::from(
                "Windows.Graphics.Capture.GraphicsCaptureItem",
            ))?
        };

        // Start all HDR monitors before waiting for a frame from any of them.
        let captures = displays
            .into_iter()
            .map(|display| MonitorCapture::start(display, &interop, &winrt))
            .collect::<windows::core::Result<Vec<_>>>()?;
        Ok((device, captures))
    };
    let (device, captures) = match start() {
        Ok(started) => started,
        // Older Windows 10 versions lack monitor capture or cursor control.
        Err(error)
            if matches!(
                error.code(),
                E_NOINTERFACE | E_NOTIMPL | REGDB_E_CLASSNOTREG
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(capture_error(error)),
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut output = if captures
        .iter()
        .any(|capture| capture.display.area == region)
    {
        Frame::new(region.width as u32, region.height as u32)
    } else {
        // Capture the SDR portions while Windows Graphics Capture takes the HDR frames.
        super::grab_gdi(region)?
    };
    let frames = captures
        .iter()
        .map(|capture| capture.next_frame(deadline))
        .collect::<windows::core::Result<Vec<_>>>()
        .map_err(capture_error)?;
    for (capture, frame) in captures.iter().zip(frames) {
        let result = copy_frame(&device, &frame, &capture.display, region, &mut output);
        let _ = frame.Close();
        result.map_err(capture_error)?;
    }
    Ok(Some(output))
}

fn capture_error(error: Error) -> io::Error {
    io::Error::other(format!("HDR screenshot failed: {error}"))
}

struct Apartment;

impl Apartment {
    fn initialize() -> windows::core::Result<Self> {
        // Keep cached WinRT factories alive across screenshot workers.
        static MTA: OnceLock<windows::core::Result<()>> = OnceLock::new();
        MTA.get_or_init(|| unsafe { CoIncrementMTAUsage().map(|_| ()) })
            .clone()?;
        // SAFETY: called on the new capture worker and balanced in Drop.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

fn create_device() -> windows::core::Result<ID3D11Device> {
    let mut device = None;
    // SAFETY: no explicit adapter and a correctly typed output pointer.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.ok_or_else(|| Error::new(E_FAIL, "missing Direct3D device"))?;
    // Capture and readback share this device across threads.
    unsafe {
        let _ = device
            .GetImmediateContext()?
            .cast::<ID3D11Multithread>()?
            .SetMultithreadProtected(true);
    }
    Ok(device)
}

struct MonitorCapture {
    display: Display,
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    arrived: mpsc::Receiver<()>,
}

impl MonitorCapture {
    fn start(
        display: Display,
        interop: &IGraphicsCaptureItemInterop,
        device: &IDirect3DDevice,
    ) -> windows::core::Result<Self> {
        // SAFETY: this handle identifies the attached DXGI monitor being captured.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(HMONITOR(display.handle as _))? };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            device,
            DirectXPixelFormat::R16G16B16A16Float,
            1,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let (signal, arrived) = mpsc::sync_channel(1);
        let capture = Self {
            display,
            session,
            pool,
            arrived,
        };
        capture
            .pool
            .FrameArrived(&TypedEventHandler::new(move |_, _| {
                let _ = signal.try_send(());
                Ok(())
            }))?;
        // Jiffshot composites its own cursor only when requested.
        capture.session.SetIsCursorCaptureEnabled(false)?;
        capture.session.StartCapture()?;
        Ok(capture)
    }

    fn next_frame(&self, deadline: Instant) -> windows::core::Result<Direct3D11CaptureFrame> {
        self.arrived
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| Error::new(E_FAIL, "timed out waiting for the monitor"))?;
        self.pool.TryGetNextFrame()
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn copy_frame(
    device: &ID3D11Device,
    frame: &Direct3D11CaptureFrame,
    display: &Display,
    region: Rect,
    output: &mut Frame,
) -> windows::core::Result<()> {
    let size = frame.ContentSize()?;
    let surface = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?;
    // SAFETY: the capture surface is backed by a Direct3D 11 texture.
    let texture: ID3D11Texture2D = unsafe { surface.GetInterface()? };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    if size.Width != display.bounds.width
        || size.Height != display.bounds.height
        || desc.Width < size.Width as u32
        || desc.Height < size.Height as u32
        || desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT
    {
        return Err(Error::new(
            E_FAIL,
            "the monitor geometry or HDR pixel format changed during capture",
        ));
    }
    let area = display.area;
    let left = (area.x - display.bounds.x) as u32;
    let top = (area.y - display.bounds.y) as u32;
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: area.width as u32,
        Height: area.height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let source_box = D3D11_BOX {
        left,
        top,
        front: 0,
        right: left + staging_desc.Width,
        bottom: top + staging_desc.Height,
        back: 1,
    };
    let mut staging = None;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: copies the in-bounds monitor intersection into a matching
    // CPU-readable texture. Mapped rows are accessed only until Unmap.
    unsafe {
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.ok_or_else(|| Error::new(E_FAIL, "missing HDR readback texture"))?;
        let context = device.GetImmediateContext()?;
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, 0, Some(&source_box));
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let stride = output.stride();
        let dest_x = (area.x - region.x) as usize * 4;
        let dest_y = (area.y - region.y) as usize;
        let width = area.width as usize;
        let exposure = 1.0 / display.white_scale;
        let lut = srgb_lut();
        for y in 0..area.height as usize {
            let row = mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize);
            let src = std::slice::from_raw_parts(row, width * 8);
            let offset = (dest_y + y) * stride + dest_x;
            tone_map_row(
                src,
                &mut output.pixels[offset..offset + width * 4],
                exposure,
                lut,
            );
        }
        context.Unmap(&staging, 0);
    }
    Ok(())
}

const SRGB_STEPS: usize = 4096;

fn srgb_lut() -> &'static [u8; SRGB_STEPS + 1] {
    static LUT: OnceLock<[u8; SRGB_STEPS + 1]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|i| {
            let linear = i as f32 / SRGB_STEPS as f32;
            let srgb = if linear <= 0.003_130_8 {
                12.92 * linear
            } else {
                1.055 * linear.powf(1.0 / 2.4) - 0.055
            };
            (srgb * 255.0).round() as u8
        })
    })
}

fn tone_map_row(src: &[u8], dst: &mut [u8], exposure: f32, lut: &[u8; SRGB_STEPS + 1]) {
    for (rgba, bgrx) in src.chunks_exact(8).zip(dst.chunks_exact_mut(4)) {
        let channel = |i: usize| {
            let linear = f16::from_le_bytes([rgba[i], rgba[i + 1]]).to_f32();
            // Ignore invalid samples, and clip negative scRGB channels to the SDR gamut.
            if linear.is_finite() {
                (linear * exposure).max(0.0)
            } else {
                0.0
            }
        };
        let (r, g, b) = (channel(0), channel(2), channel(4));
        let peak = r.max(g).max(b);
        // Leave SDR midtones alone. A smooth shoulder above 90% linear white
        // reserves the top of the SDR range for HDR highlights.
        const KNEE: f32 = 0.9;
        let scale = if peak > KNEE {
            let excess = peak - KNEE;
            (KNEE + excess / (1.0 + excess / (1.0 - KNEE))) / peak
        } else {
            1.0
        };
        let encode = |value: f32| {
            let i = (value * scale * SRGB_STEPS as f32).round() as usize;
            lut[i.min(SRGB_STEPS)]
        };
        bgrx.copy_from_slice(&[encode(b), encode(g), encode(r), 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(rgba: [f32; 4], white_scale: f32) -> [u8; 4] {
        let bytes = rgba.map(|value| f16::from_f32(value).to_le_bytes());
        let mut pixel = [0; 4];
        tone_map_row(
            bytes.as_flattened(),
            &mut pixel,
            1.0 / white_scale,
            srgb_lut(),
        );
        pixel
    }

    #[test]
    fn sdr_colors_are_normalized_to_the_monitor_white_level() {
        let color = [0.5, 0.25, 0.0, 0.5];
        assert_eq!(convert(color, 1.0), [0, 137, 188, 255]);
        assert_eq!(convert(color.map(|v| v * 3.0), 3.0), [0, 137, 188, 255]);
    }

    #[test]
    fn hdr_highlights_stay_brighter_than_sdr_white() {
        let white = convert([1.0; 4], 1.0)[0];
        let highlight = convert([5.0; 4], 1.0)[0];
        assert!((245..=252).contains(&white));
        assert!(highlight > white);
        assert_eq!(convert([0.0, -0.5, f32::NAN, 0.0], 1.0), [0, 0, 0, 255]);
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop with HDR enabled"]
    fn captures_a_live_hdr_monitor() {
        super::super::enable_per_monitor_dpi();
        let desktop = super::super::virtual_screen();
        let targets = displays(desktop);
        assert!(!targets.is_empty(), "Windows reports no active HDR display");
        for target in targets {
            let (x, y) = target.bounds.center();
            let region = Rect::new(x, y, 32, 16);
            let frame = grab(region).unwrap().expect("HDR capture is supported");
            frame.validate().unwrap();
            assert_eq!(frame.bounds(), Rect::new(0, 0, 32, 16));
            println!(
                "Captured HDR monitor at {x}, {y} with SDR white scale {}",
                target.white_scale
            );
        }
        super::super::grab(desktop).unwrap().validate().unwrap();
    }
}
