//! Renderer selection.
//!
//! Studio must start inside a Windows 11 ARM64 VM (Parallels on Apple
//! Silicon) where Direct3D 12 may be missing and GPU support is limited.
//! Strategy: wgpu over DX12 | Vulkan | GL with an adapter selector that logs
//! every adapter and prefers real GPUs, then a retry with glow (OpenGL) if
//! the wgpu app cannot be created. `--renderer wgpu|glow`, `U_ROME_RENDERER`
//! and the persisted setting pin a single renderer.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
use eframe::wgpu;
use serde::{Deserialize, Serialize};

use crate::diagnostics::RenderInfo;

/// Environment variable selecting the renderer.
pub const RENDERER_ENV: &str = "U_ROME_RENDERER";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererPref {
    /// wgpu first, glow if wgpu fails.
    #[default]
    Auto,
    Wgpu,
    Glow,
}

impl RendererPref {
    pub const ALL: [RendererPref; 3] = [RendererPref::Auto, RendererPref::Wgpu, RendererPref::Glow];

    pub fn label(self) -> &'static str {
        match self {
            RendererPref::Auto => "Auto (wgpu, then OpenGL)",
            RendererPref::Wgpu => "wgpu (DX12 / Vulkan / GL)",
            RendererPref::Glow => "OpenGL (glow)",
        }
    }

    /// Renderers to try, in order.
    pub fn plan(self) -> Vec<RendererKind> {
        match self {
            RendererPref::Auto => vec![RendererKind::Wgpu, RendererKind::Glow],
            RendererPref::Wgpu => vec![RendererKind::Wgpu],
            RendererPref::Glow => vec![RendererKind::Glow],
        }
    }
}

impl FromStr for RendererPref {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "wgpu" => Ok(Self::Wgpu),
            "glow" | "gl" | "opengl" => Ok(Self::Glow),
            other => Err(format!(
                "unknown renderer {other:?} (expected auto, wgpu or glow)"
            )),
        }
    }
}

/// A concrete renderer attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererKind {
    Wgpu,
    Glow,
}

impl fmt::Display for RendererKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RendererKind::Wgpu => "wgpu",
            RendererKind::Glow => "glow",
        })
    }
}

impl RendererKind {
    pub fn to_eframe(self) -> eframe::Renderer {
        match self {
            RendererKind::Wgpu => eframe::Renderer::Wgpu,
            RendererKind::Glow => eframe::Renderer::Glow,
        }
    }
}

/// Resolves the preference: CLI beats env beats settings. Invalid values are
/// reported and skipped. Returns the preference and where it came from.
pub fn resolve(
    cli: Option<&str>,
    env: Option<&str>,
    settings: RendererPref,
) -> (RendererPref, String, Vec<String>) {
    let mut warnings = Vec::new();
    if let Some(v) = cli {
        match v.parse() {
            Ok(p) => return (p, "--renderer".into(), warnings),
            Err(e) => warnings.push(format!("--renderer: {e}")),
        }
    }
    if let Some(v) = env.filter(|v| !v.trim().is_empty()) {
        match v.parse() {
            Ok(p) => return (p, RENDERER_ENV.into(), warnings),
            Err(e) => warnings.push(format!("{RENDERER_ENV}: {e}")),
        }
    }
    let source = if settings == RendererPref::Auto {
        "auto"
    } else {
        "settings"
    };
    (settings, source.into(), warnings)
}

/// Extracts `--renderer X` / `--renderer=X` from process arguments.
pub fn cli_renderer(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--renderer" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--renderer=") {
            return Some(v.to_owned());
        }
    }
    None
}

/// wgpu backends to enable on this platform. `WGPU_BACKEND` overrides.
pub fn wgpu_backends() -> wgpu::Backends {
    if let Some(b) = wgpu::Backends::from_env() {
        log::info!("WGPU_BACKEND override: {b:?}");
        return b;
    }
    if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::DX12 | wgpu::Backends::VULKAN | wgpu::Backends::GL
    }
}

/// Ranks an adapter: higher is better. Hardware beats virtual beats software;
/// among equals, DX12 > Metal > Vulkan > GL (GL drivers in VMs are often the
/// least reliable for wgpu, but still better than nothing).
pub fn adapter_score(device: wgpu::DeviceType, backend: wgpu::Backend) -> u32 {
    let d = match device {
        wgpu::DeviceType::DiscreteGpu => 50,
        wgpu::DeviceType::IntegratedGpu => 40,
        wgpu::DeviceType::VirtualGpu => 30,
        wgpu::DeviceType::Other => 20,
        wgpu::DeviceType::Cpu => 10,
    };
    let b = match backend {
        wgpu::Backend::Dx12 => 4,
        wgpu::Backend::Metal => 4,
        wgpu::Backend::Vulkan => 3,
        wgpu::Backend::Gl => 2,
        _ => 1,
    };
    d * 10 + b
}

pub fn describe_adapter(info: &wgpu::AdapterInfo) -> String {
    format!(
        "{} [{:?}, {:?}, vendor {:#06x}, device {:#06x}, driver {} {}]",
        info.name,
        info.backend,
        info.device_type,
        info.vendor,
        info.device,
        info.driver,
        info.driver_info
    )
}

/// eframe wgpu configuration with our backends, adapter selector and modest
/// device limits (egui needs little; VM adapters often report low limits).
pub fn wgpu_configuration() -> WgpuConfiguration {
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = wgpu_backends();
    setup.native_adapter_selector = Some(Arc::new(|adapters, surface| {
        log::info!("wgpu enumerated {} adapter(s)", adapters.len());
        let mut best: Option<(&wgpu::Adapter, u32)> = None;
        for a in adapters {
            let info = a.get_info();
            let supported = surface.is_none_or(|s| a.is_surface_supported(s));
            let score = adapter_score(info.device_type, info.backend);
            log::info!(
                "  adapter: {} (score {score}, surface supported: {supported})",
                describe_adapter(&info)
            );
            if supported && best.is_none_or(|(_, s)| score > s) {
                best = Some((a, score));
            }
        }
        match best {
            Some((a, _)) => {
                log::info!("selected adapter: {}", describe_adapter(&a.get_info()));
                Ok(a.clone())
            }
            None if adapters.is_empty() => Err(
                "no wgpu adapters found for the enabled backends (DX12/Vulkan/GL); \
                 the GPU driver may not support them"
                    .into(),
            ),
            None => Err(format!(
                "none of the {} wgpu adapter(s) can present to this window",
                adapters.len()
            )),
        }
    }));
    setup.device_descriptor = Arc::new(|adapter| {
        let supported = adapter.limits();
        let base = if adapter.get_info().backend == wgpu::Backend::Gl {
            wgpu::Limits::downlevel_webgl2_defaults()
        } else {
            wgpu::Limits::downlevel_defaults()
        };
        wgpu::DeviceDescriptor {
            label: Some("rome studio device"),
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: supported.max_texture_dimension_2d.min(8192),
                ..base
            }
            .using_resolution(supported),
            ..Default::default()
        }
    });
    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// Fills renderer facts from an eframe creation context.
pub fn render_info_from(cc: &eframe::CreationContext<'_>) -> RenderInfo {
    if let Some(rs) = &cc.wgpu_render_state {
        let info = rs.adapter.get_info();
        return RenderInfo {
            renderer: "wgpu".into(),
            backend: format!("{:?}", info.backend),
            adapter: info.name.clone(),
            device_type: format!("{:?}", info.device_type),
            driver: format!("{} {}", info.driver, info.driver_info)
                .trim()
                .to_owned(),
            available_adapters: rs
                .available_adapters
                .iter()
                .map(|a| describe_adapter(&a.get_info()))
                .collect(),
            ..Default::default()
        };
    }
    if let Some(gl) = &cc.gl {
        use eframe::glow::HasContext as _;
        // SAFETY: plain glGetString queries on the context eframe just made
        // current on this thread.
        let (renderer, vendor, version) = unsafe {
            (
                gl.get_parameter_string(eframe::glow::RENDERER),
                gl.get_parameter_string(eframe::glow::VENDOR),
                gl.get_parameter_string(eframe::glow::VERSION),
            )
        };
        return RenderInfo {
            renderer: "glow".into(),
            backend: "OpenGL".into(),
            adapter: renderer,
            device_type: vendor,
            driver: version,
            ..Default::default()
        };
    }
    RenderInfo {
        renderer: "unknown".into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prefs() {
        assert_eq!("WGPU".parse(), Ok(RendererPref::Wgpu));
        assert_eq!("opengl".parse(), Ok(RendererPref::Glow));
        assert_eq!("".parse(), Ok(RendererPref::Auto));
        assert!("dx9".parse::<RendererPref>().is_err());
    }

    #[test]
    fn precedence() {
        let (p, src, w) = resolve(Some("glow"), Some("wgpu"), RendererPref::Auto);
        assert_eq!(
            (p, src.as_str(), w.len()),
            (RendererPref::Glow, "--renderer", 0)
        );
        let (p, src, _) = resolve(None, Some("wgpu"), RendererPref::Glow);
        assert_eq!((p, src.as_str()), (RendererPref::Wgpu, RENDERER_ENV));
        let (p, src, w) = resolve(Some("bogus"), None, RendererPref::Glow);
        assert_eq!(
            (p, src.as_str(), w.len()),
            (RendererPref::Glow, "settings", 1)
        );
        let (p, src, _) = resolve(None, Some(" "), RendererPref::Auto);
        assert_eq!((p, src.as_str()), (RendererPref::Auto, "auto"));
    }

    #[test]
    fn plans() {
        assert_eq!(
            RendererPref::Auto.plan(),
            vec![RendererKind::Wgpu, RendererKind::Glow]
        );
        assert_eq!(RendererPref::Glow.plan(), vec![RendererKind::Glow]);
    }

    #[test]
    fn cli_args() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            cli_renderer(&a(&["x", "--renderer", "glow"])),
            Some("glow".into())
        );
        assert_eq!(
            cli_renderer(&a(&["x", "--renderer=wgpu"])),
            Some("wgpu".into())
        );
        assert_eq!(cli_renderer(&a(&["x", "--renderer"])), None);
        assert_eq!(cli_renderer(&a(&["x"])), None);
    }

    #[test]
    fn adapter_ranking() {
        use wgpu::{Backend, DeviceType};
        let hw_dx = adapter_score(DeviceType::IntegratedGpu, Backend::Dx12);
        let hw_gl = adapter_score(DeviceType::IntegratedGpu, Backend::Gl);
        let warp = adapter_score(DeviceType::Cpu, Backend::Dx12);
        let virt = adapter_score(DeviceType::VirtualGpu, Backend::Gl);
        assert!(hw_dx > hw_gl);
        assert!(hw_gl > virt);
        assert!(virt > warp);
    }
}
