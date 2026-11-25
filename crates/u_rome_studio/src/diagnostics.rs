//! Runtime facts the user can paste into a bug report (Help > Copy diagnostics)
//! and that the panic hook embeds in crash reports.

use std::sync::RwLock;

use crate::logging;

/// What the renderer setup ended up with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderInfo {
    /// `wgpu` or `glow`.
    pub renderer: String,
    /// Graphics API in use (`Dx12`, `Vulkan`, `Gl`, `Metal`, `OpenGL`).
    pub backend: String,
    /// Adapter / GL renderer string.
    pub adapter: String,
    /// Adapter device type (`DiscreteGpu`, `Cpu`, ...) or GL vendor.
    pub device_type: String,
    /// Driver name and version.
    pub driver: String,
    /// Where the renderer choice came from (`auto`, `--renderer`, env, settings).
    pub requested: String,
    /// Errors from renderer attempts that failed before this one.
    pub failed_attempts: Vec<String>,
    /// All adapters wgpu enumerated (wgpu only).
    pub available_adapters: Vec<String>,
}

static RENDER: RwLock<Option<RenderInfo>> = RwLock::new(None);
static BACKEND_LABEL: RwLock<Option<String>> = RwLock::new(None);

pub fn set_render_info(info: RenderInfo) {
    if let Ok(mut g) = RENDER.write() {
        *g = Some(info);
    }
}

pub fn render_info() -> Option<RenderInfo> {
    RENDER.read().ok().and_then(|g| g.clone())
}

/// Records failed renderer attempts before a render context exists.
pub fn push_failed_attempt(msg: String) {
    if let Ok(mut g) = RENDER.write() {
        g.get_or_insert_with(RenderInfo::default)
            .failed_attempts
            .push(msg);
    }
}

pub fn set_backend_label(label: String) {
    if let Ok(mut g) = BACKEND_LABEL.write() {
        *g = Some(label);
    }
}

/// A diagnostics snapshot.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub version: String,
    pub os: String,
    pub build_arch: String,
    pub cpu: String,
    pub render: Option<RenderInfo>,
    pub data_backend: Option<String>,
    pub data_dir: String,
    pub log_file: Option<String>,
    pub extra: Vec<(String, String)>,
    pub recent_log: Vec<String>,
}

impl Diagnostics {
    /// Collects a snapshot including the last `log_lines` log lines.
    pub fn collect(log_lines: usize) -> Self {
        Self {
            version: format!("{} {}", crate::APP_NAME, crate::VERSION),
            os: os_description(),
            build_arch: format!(
                "{} ({}{})",
                std::env::consts::ARCH,
                std::env::consts::OS,
                if cfg!(debug_assertions) {
                    ", debug build"
                } else {
                    ""
                }
            ),
            cpu: cpu_description(),
            render: render_info(),
            data_backend: BACKEND_LABEL.read().ok().and_then(|g| g.clone()),
            data_dir: crate::paths::data_dir().display().to_string(),
            log_file: logging::log_file_path().map(|p| p.display().to_string()),
            extra: Vec::new(),
            recent_log: logging::buffer().tail_text(log_lines),
        }
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let mut kv = |k: &str, v: &str| {
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
            s.push('\n');
        };
        kv("Version", &self.version);
        kv("OS", &self.os);
        kv("Build", &self.build_arch);
        kv("CPU", &self.cpu);
        match &self.render {
            Some(r) => {
                kv("Renderer", &r.renderer);
                kv("Requested", &r.requested);
                kv("Graphics API", &r.backend);
                kv("Adapter", &r.adapter);
                kv("Device type", &r.device_type);
                kv("Driver", &r.driver);
                for a in &r.available_adapters {
                    kv("Available adapter", a);
                }
                for f in &r.failed_attempts {
                    kv("Failed attempt", f);
                }
            }
            None => kv("Renderer", "not initialised"),
        }
        kv(
            "Data backend",
            self.data_backend.as_deref().unwrap_or("none"),
        );
        kv("Data dir", &self.data_dir);
        kv("Log file", self.log_file.as_deref().unwrap_or("none"));
        for (k, v) in &self.extra {
            kv(k, v);
        }
        s.push_str(&format!(
            "\n--- last {} log lines ---\n",
            self.recent_log.len()
        ));
        for l in &self.recent_log {
            s.push_str(l);
            s.push('\n');
        }
        s
    }
}

/// OS family plus whatever version hints the environment exposes.
pub fn os_description() -> String {
    let mut s = format!("{} / {}", std::env::consts::OS, std::env::consts::FAMILY);
    if cfg!(windows) {
        // Under x64 emulation on ARM64 Windows, PROCESSOR_ARCHITECTURE reports
        // the emulated arch; PROCESSOR_ARCHITEW6432 (if set) the native one.
        for var in ["PROCESSOR_ARCHITECTURE", "PROCESSOR_ARCHITEW6432"] {
            if let Ok(v) = std::env::var(var) {
                s.push_str(&format!(", {var}={v}"));
            }
        }
    }
    s
}

fn cpu_description() -> String {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    match std::env::var("PROCESSOR_IDENTIFIER") {
        Ok(id) => format!("{id}, {threads} threads"),
        Err(_) => format!("{threads} threads"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_contains_key_fields() {
        let d = Diagnostics {
            version: "ROME Studio 0.1.0".into(),
            os: "windows / windows".into(),
            build_arch: "aarch64".into(),
            cpu: "8 threads".into(),
            render: Some(RenderInfo {
                renderer: "glow".into(),
                backend: "OpenGL".into(),
                adapter: "Parallels Display Adapter".into(),
                failed_attempts: vec!["wgpu: no suitable adapter".into()],
                ..Default::default()
            }),
            recent_log: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        let t = d.to_text();
        assert!(t.contains("Renderer: glow"));
        assert!(t.contains("Adapter: Parallels Display Adapter"));
        assert!(t.contains("Failed attempt: wgpu: no suitable adapter"));
        assert!(t.contains("--- last 2 log lines ---\na\nb\n"));
        assert!(t.contains("Data backend: none"));
    }
}
