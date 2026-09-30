//! The displays connected to this PC (for choosing where the precise pattern window opens).
//! Windows: EnumDisplayMonitors; Linux: `xrandr --listmonitors`. Coordinates are physical pixels on the
//! virtual desktop.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
}

impl Display {
    pub fn label(&self, index: usize) -> String {
        format!("{}: {} · {}×{} at {},{}{}", index + 1, self.name, self.w, self.h, self.x, self.y, if self.primary { " · primary" } else { "" })
    }
}

#[cfg(target_os = "windows")]
pub fn list() -> Vec<Display> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};

    unsafe extern "system" fn each(mon: HMONITOR, _dc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<Display>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(mon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            out.push(Display {
                name: String::from_utf16_lossy(&info.szDevice[..len]).trim_start_matches(r"\\.\").to_string(),
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
                primary: info.monitorInfo.dwFlags & 1 == 1, // MONITORINFOF_PRIMARY
            });
        }
        BOOL(1)
    }

    let mut out: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut out as *mut Vec<Display> as isize));
    }
    out
}

#[cfg(not(target_os = "windows"))]
pub fn list() -> Vec<Display> {
    std::process::Command::new("xrandr")
        .arg("--listmonitors")
        .output()
        .ok()
        .map(|o| parse_xrandr(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// " 0: +*DP-1 2560/597x1440/336+0+0  DP-1" -> DP-1, 2560×1440 at 0,0, primary
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub fn parse_xrandr(out: &str) -> Vec<Display> {
    out.lines()
        .filter_map(|l| {
            let (_, rest) = l.trim().split_once(": ")?;
            let mut parts = rest.split_whitespace();
            let flagged = parts.next()?;
            let geom = parts.next()?;
            let name = parts.next().unwrap_or(flagged.trim_start_matches(['+', '*'])).to_string();
            // W/mmxH/mm+X+Y
            let (size, pos) = geom.split_once('+')?;
            let (w, h) = size.split_once('x')?;
            let num = |s: &str| s.split('/').next()?.parse::<i32>().ok();
            let mut xy = pos.split('+');
            Some(Display {
                name,
                w: num(w)?,
                h: num(h)?,
                x: xy.next()?.parse().ok()?,
                y: xy.next()?.parse().ok()?,
                primary: flagged.contains('*'),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrandr_monitors_are_parsed() {
        let out = "Monitors: 2\n 0: +*DP-1 2560/597x1440/336+0+0  DP-1\n 1: +HDMI-1 1920/527x1080/296+2560+180  HDMI-1\n";
        let d = parse_xrandr(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], Display { name: "DP-1".into(), x: 0, y: 0, w: 2560, h: 1440, primary: true });
        assert_eq!((d[1].x, d[1].y, d[1].w, d[1].primary), (2560, 180, 1920, false));
        assert_eq!(d[1].label(1), "2: HDMI-1 · 1920×1080 at 2560,180");
        let _ = list(); // must not fail without a display
    }
}
