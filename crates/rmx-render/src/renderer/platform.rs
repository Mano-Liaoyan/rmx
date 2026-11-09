//! The graphics platform that is used by the renderer.

use std::num::NonZeroU32;

use glutin::config::{ColorBufferType, Config, ConfigTemplateBuilder, GetGlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentContext, PossiblyCurrentContext,
    Robustness, Version,
};
use glutin::display::{Display, DisplayApiPreference, DisplayFeatures, GetGlDisplay};
use glutin::error::Result as GlutinResult;
use glutin::prelude::*;
use glutin::surface::{Surface, SurfaceAttributesBuilder, WindowSurface};
use log::{LevelFilter, debug};

use winit::dpi::PhysicalSize;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use winit::platform::x11;
use winit::raw_window_handle::{RawDisplayHandle, RawWindowHandle};

/// Create the GL display.
pub fn create_gl_display(
    raw_display_handle: RawDisplayHandle,
    _raw_window_handle: Option<RawWindowHandle>,
    _prefer_egl: bool,
) -> GlutinResult<Display> {
    #[cfg(target_os = "macos")]
    let preference = DisplayApiPreference::Cgl;

    #[cfg(windows)]
    let preference = if _prefer_egl {
        DisplayApiPreference::EglThenWgl(Some(_raw_window_handle.unwrap()))
    } else {
        DisplayApiPreference::WglThenEgl(Some(_raw_window_handle.unwrap()))
    };

    #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
    let preference = if _prefer_egl {
        DisplayApiPreference::EglThenGlx(Box::new(x11::register_xlib_error_hook))
    } else {
        DisplayApiPreference::GlxThenEgl(Box::new(x11::register_xlib_error_hook))
    };

    #[cfg(all(not(feature = "x11"), not(any(target_os = "macos", windows))))]
    let preference = DisplayApiPreference::Egl;

    let display = unsafe { Display::new(raw_display_handle, preference)? };
    log::info!("Using {}", { display.version_string() });
    Ok(display)
}

pub fn pick_gl_config(
    gl_display: &Display,
    raw_window_handle: Option<RawWindowHandle>,
) -> Result<Config, String> {
    let mut default_config = ConfigTemplateBuilder::new()
        .with_depth_size(0)
        .with_stencil_size(0)
        .with_transparency(true);

    if let Some(raw_window_handle) = raw_window_handle {
        default_config = default_config.compatible_with_native_window(raw_window_handle);
    }

    let config_10bit = default_config
        .clone()
        .with_buffer_type(ColorBufferType::Rgb { r_size: 10, g_size: 10, b_size: 10 })
        .with_alpha_size(2);

    let configs = [
        default_config.clone(),
        config_10bit.clone(),
        default_config.with_transparency(false),
        config_10bit.with_transparency(false),
    ];

    for config in configs {
        let gl_config = unsafe {
            gl_display.find_configs(config.build()).ok().and_then(|mut configs| configs.next())
        };

        if let Some(gl_config) = gl_config {
            debug!(
                r#"Picked GL Config:
  buffer_type: {:?}
  alpha_size: {}
  num_samples: {}
  hardware_accelerated: {:?}
  supports_transparency: {:?}
  config_api: {:?}
  srgb_capable: {}"#,
                gl_config.color_buffer_type(),
                gl_config.alpha_size(),
                gl_config.num_samples(),
                gl_config.hardware_accelerated(),
                gl_config.supports_transparency(),
                gl_config.api(),
                gl_config.srgb_capable(),
            );

            return Ok(gl_config);
        }
    }

    Err(String::from("failed to find suitable GL configuration."))
}

pub fn create_gl_context(
    gl_display: &Display,
    gl_config: &Config,
    raw_window_handle: Option<RawWindowHandle>,
    share: Option<&PossiblyCurrentContext>,
) -> GlutinResult<NotCurrentContext> {
    let debug = log::max_level() >= LevelFilter::Debug;

    let apis = [
        (ContextApi::OpenGl(Some(Version::new(3, 3))), GlProfile::Core),
        // Try gles before OpenGL 2.1 as it tends to be more stable.
        (ContextApi::Gles(Some(Version::new(2, 0))), GlProfile::Core),
        (ContextApi::OpenGl(Some(Version::new(2, 1))), GlProfile::Compatibility),
    ];

    let robustness = gl_display.supported_features().contains(DisplayFeatures::CONTEXT_ROBUSTNESS);
    let robustness: &[Robustness] = if robustness {
        &[Robustness::RobustLoseContextOnReset, Robustness::NotRobust]
    } else {
        &[Robustness::NotRobust]
    };

    // Find the first context that builds without any errors.
    let mut error = None;
    for (api, profile) in apis {
        for robustness in robustness {
            let mut builder = ContextAttributesBuilder::new();
            if let Some(share) = share {
                builder = builder.with_sharing(share);
            }
            let attributes = builder
                .with_debug(debug)
                .with_context_api(api)
                .with_profile(profile)
                .with_robustness(*robustness)
                .build(raw_window_handle);

            match unsafe { gl_display.create_context(gl_config, &attributes) } {
                Ok(profile) => return Ok(profile),
                Err(err) => error = Some(err),
            }
        }
    }

    // If no context was built successfully, return an error for the most permissive one.
    Err(error.unwrap())
}

pub fn create_gl_surface(
    gl_context: &NotCurrentContext,
    size: PhysicalSize<u32>,
    raw_window_handle: RawWindowHandle,
) -> GlutinResult<Surface<WindowSurface>> {
    // Get the display and the config used to create that context.
    let gl_display = gl_context.display();
    let gl_config = gl_context.config();

    let surface_attributes =
        SurfaceAttributesBuilder::<WindowSurface>::new().with_srgb(Some(false)).build(
            raw_window_handle,
            NonZeroU32::new(size.width).unwrap(),
            NonZeroU32::new(size.height).unwrap(),
        );

    // Create the GL surface to draw into.
    unsafe { gl_display.create_window_surface(&gl_config, &surface_attributes) }
}

/// Have the driver allocate the back buffer of the surface just made current, which it would
/// otherwise do in the middle of the first frame.
pub fn touch_back_buffer(context: &PossiblyCurrentContext) {
    super::load_gl(context);
    unsafe {
        crate::gl::Clear(crate::gl::COLOR_BUFFER_BIT);
        crate::gl::Flush();
    }
}

/// Load the driver's libraries, which glvnd would load one after another once there is a display.
/// Touching EGL itself from a second thread is no good: NVIDIA's driver then takes a slower,
/// locked path, which costs its first swap more than this saves.
///
/// Best effort: a library that does not load is left for EGL to report.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn preload_driver() {
    use std::ffi::{CString, c_char, c_int, c_void};

    const RTLD_NOW: c_int = 2;
    unsafe extern "C" {
        fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    }

    const MANIFESTS: [&str; 3] = [
        "/usr/share/glvnd/egl_vendor.d",
        "/etc/glvnd/egl_vendor.d",
        "/usr/share/egl/egl_external_platform.d",
    ];
    for manifest in MANIFESTS.iter().filter_map(|dir| std::fs::read_dir(dir).ok()).flatten() {
        let Ok(text) = std::fs::read_to_string(manifest.map_or_else(|_| "".into(), |m| m.path()))
        else {
            continue;
        };
        for library in text.lines().filter_map(library_path).filter_map(|l| CString::new(l).ok()) {
            // Never closed: EGL is about to want them.
            unsafe { dlopen(library.as_ptr(), RTLD_NOW) };
        }
    }
}

/// The library named by a line of a glvnd or EGL external platform manifest, if it names one.
#[cfg(not(any(target_os = "macos", windows)))]
fn library_path(line: &str) -> Option<&str> {
    let (_, value) = line.split_once("\"library_path\"")?;
    value.split('"').nth(1)
}

#[cfg(all(test, not(any(target_os = "macos", windows))))]
mod tests {
    use super::library_path;

    #[test]
    fn finds_the_library_of_a_manifest_line() {
        let line = r#"        "library_path" : "libEGL_nvidia.so.0""#;
        assert_eq!(library_path(line), Some("libEGL_nvidia.so.0"));
        assert_eq!(library_path(r#"    "file_format_version" : "1.0.0","#), None);
    }
}
