//! How the windows are drawn: Vulkan through wgpu when a Vulkan GPU is present (low latency: one
//! queued frame, Immediate / Mailbox presentation without VSync, FIFO with VSync for the pattern
//! window), otherwise OpenGL (glow) as before. `--renderer vulkan|opengl|auto` overrides the choice.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Auto,
    Vulkan,
    OpenGl,
}

pub fn parse(args: &[String]) -> Choice {
    match args.iter().position(|a| a == "--renderer").and_then(|i| args.get(i + 1)).map(|s| s.to_lowercase()) {
        Some(s) if s == "vulkan" || s == "wgpu" => Choice::Vulkan,
        Some(s) if s == "opengl" || s == "gl" || s == "glow" => Choice::OpenGl,
        _ => Choice::Auto,
    }
}

/// Is there a Vulkan driver with at least one GPU? (checked before asking wgpu for Vulkan)
pub fn vulkan_available() -> bool {
    unsafe {
        let Ok(entry) = ash::Entry::load() else { return false };
        let app = ash::vk::ApplicationInfo::builder().api_version(ash::vk::make_api_version(0, 1, 1, 0));
        let info = ash::vk::InstanceCreateInfo::builder().application_info(&app);
        let Ok(instance) = entry.create_instance(&info, None) else { return false };
        let ok = instance.enumerate_physical_devices().map(|d| !d.is_empty()).unwrap_or(false);
        instance.destroy_instance(None);
        ok
    }
}

/// Renderer attempts in order: (options, short name). `vsync` = present on refreshes (pattern window).
pub fn attempts(choice: Choice, vsync: bool, viewport: egui::ViewportBuilder) -> Vec<(eframe::NativeOptions, &'static str)> {
    let vulkan = || {
        let wgpu_options = egui_wgpu::WgpuConfiguration {
            supported_backends: wgpu::Backends::VULKAN,
            // no VSync: Immediate, else Mailbox, else FIFO; with VSync: FIFO (always supported)
            present_mode: if vsync { wgpu::PresentMode::Fifo } else { wgpu::PresentMode::AutoNoVsync },
            // one frame in flight: what is drawn is what the display gets next
            desired_maximum_frame_latency: Some(1),
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        };
        (
            eframe::NativeOptions {
                vsync,
                renderer: eframe::Renderer::Wgpu,
                wgpu_options,
                viewport: viewport.clone(),
                ..Default::default()
            },
            "vulkan",
        )
    };
    let gl = || {
        (eframe::NativeOptions { vsync, renderer: eframe::Renderer::Glow, viewport: viewport.clone(), ..Default::default() }, "opengl")
    };
    match choice {
        Choice::OpenGl => vec![gl()],
        Choice::Vulkan => vec![vulkan(), gl()],
        Choice::Auto if vulkan_available() => vec![vulkan(), gl()],
        Choice::Auto => vec![gl()],
    }
}

/// Run `make_app` with the first renderer that starts; returns the name of the one that ran
pub fn run_with_fallback(
    title: &str,
    choice: Choice,
    vsync: bool,
    viewport: egui::ViewportBuilder,
    make_app: impl Fn(&eframe::CreationContext<'_>) -> Box<dyn eframe::App>,
) -> anyhow::Result<()> {
    let mut last_err = None;
    for (options, name) in attempts(choice, vsync, viewport) {
        let make = &make_app;
        match eframe::run_native(title, options, Box::new(move |cc| Ok(make(cc)))) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("{} renderer could not start ({}), trying the next one", name, e);
                last_err = Some(e);
            }
        }
    }
    Err(anyhow::anyhow!("GUI error: {}", last_err.map(|e| e.to_string()).unwrap_or_default()))
}

/// What is drawing this window, for the Dashboard / Input tab / pattern window
pub fn describe(cc: &eframe::CreationContext<'_>, vsync: bool) -> String {
    if let Some(rs) = cc.wgpu_render_state.as_ref() {
        let i = rs.adapter.get_info();
        return format!(
            "{:?} via wgpu on {} ({:?}) · {} · 1 frame queued",
            i.backend,
            i.name,
            i.device_type,
            if vsync { "FIFO presentation (VSync, every frame shown for whole refreshes)" } else { "Immediate/Mailbox presentation (no VSync, lowest latency)" }
        );
    }
    if let Some(gl) = cc.gl.as_ref() {
        use eframe::glow::HasContext;
        let (renderer, version) = unsafe { (gl.get_parameter_string(eframe::glow::RENDERER), gl.get_parameter_string(eframe::glow::VERSION)) };
        return format!(
            "OpenGL (glow) on {} · {} · {}",
            renderer,
            version,
            if vsync { "VSync on" } else { "VSync off" }
        );
    }
    "unknown renderer".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_flag() {
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(parse(&a("--renderer vulkan")), Choice::Vulkan);
        assert_eq!(parse(&a("--renderer opengl")), Choice::OpenGl);
        assert_eq!(parse(&a("")), Choice::Auto);
    }

    #[test]
    fn vulkan_is_tried_first_and_opengl_is_the_fallback() {
        let names = |c| attempts(c, false, egui::ViewportBuilder::default()).into_iter().map(|a| a.1).collect::<Vec<_>>();
        assert_eq!(names(Choice::Vulkan), vec!["vulkan", "opengl"]);
        assert_eq!(names(Choice::OpenGl), vec!["opengl"]);
        let auto = names(Choice::Auto);
        assert_eq!(auto.last(), Some(&"opengl"));
        let (o, _) = attempts(Choice::Vulkan, true, egui::ViewportBuilder::default()).remove(0);
        assert_eq!(o.wgpu_options.present_mode, wgpu::PresentMode::Fifo);
        assert_eq!(o.wgpu_options.desired_maximum_frame_latency, Some(1));
    }
}
