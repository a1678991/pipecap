//! LaunchAgent management: keep a cap applied across logins, sleep/wake and
//! cable re-plugs by running `pipecap watch` in the background.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

extern "C" {
    fn getuid() -> u32;
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { getuid() }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn label(id: &str) -> String {
    format!("io.github.pipecap.{}", id.to_ascii_lowercase())
}

pub fn plist_path(id: &str) -> Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(id))))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/Logs/pipecap.log"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the LaunchAgent property list.
pub fn render_plist(label: &str, args: &[String], log: &str) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    s.push_str(&format!(
        "  <key>Label</key><string>{}</string>\n",
        xml_escape(label)
    ));
    s.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for a in args {
        s.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    s.push_str("  </array>\n");
    s.push_str("  <key>RunAtLoad</key><true/>\n");
    s.push_str("  <key>KeepAlive</key><true/>\n");
    s.push_str("  <key>ProcessType</key><string>Background</string>\n");
    s.push_str(&format!(
        "  <key>StandardOutPath</key><string>{}</string>\n",
        xml_escape(log)
    ));
    s.push_str(&format!(
        "  <key>StandardErrorPath</key><string>{}</string>\n",
        xml_escape(log)
    ));
    s.push_str("</dict>\n</plist>\n");
    s
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")
}

/// Write the plist and load it with launchd.
pub fn install(id: &str, max_hz: f64, interval_secs: u64) -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let path = plist_path(id)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let args = vec![
        exe.to_string_lossy().into_owned(),
        "watch".into(),
        "--display".into(),
        id.into(),
        "--max-hz".into(),
        format!("{max_hz}"),
        "--interval".into(),
        interval_secs.to_string(),
    ];
    let lbl = label(id);
    std::fs::write(
        &path,
        render_plist(&lbl, &args, &log_path()?.to_string_lossy()),
    )?;
    let target = format!("gui/{}", uid());
    // Unload a previous copy first (ignore errors), then bootstrap.
    let _ = launchctl(&["bootout", &format!("{target}/{lbl}")]);
    let out = launchctl(&["bootstrap", &target, &path.to_string_lossy()])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(path)
}

pub fn uninstall(id: &str) -> Result<PathBuf> {
    let path = plist_path(id)?;
    let _ = launchctl(&["bootout", &format!("gui/{}/{}", uid(), label(id))]);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(path)
}

/// `Some(true)` when loaded, `Some(false)` when the plist exists but is not loaded.
pub fn status(id: &str) -> Result<(PathBuf, bool)> {
    let path = plist_path(id)?;
    let out = launchctl(&["print", &format!("gui/{}/{}", uid(), label(id))])?;
    Ok((path, out.status.success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_well_formed() {
        let p = render_plist(
            "io.github.pipecap.test",
            &["/bin/x".into(), "a<b".into()],
            "/tmp/l",
        );
        assert!(p.contains("<string>a&lt;b</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.ends_with("</plist>\n"));
    }

    #[test]
    fn label_is_lowercase() {
        assert_eq!(label("ABCD1234"), "io.github.pipecap.abcd1234");
    }
}
