use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

#[cfg(target_os = "macos")]
const CANDIDATES: &[(&str, &[&str])] = &[("pbcopy", &[])];

#[cfg(target_os = "windows")]
const CANDIDATES: &[(&str, &[&str])] = &[("clip.exe", &[])];

#[cfg(all(unix, not(target_os = "macos")))]
const CANDIDATES: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

#[cfg(not(any(unix, target_os = "windows")))]
const CANDIDATES: &[(&str, &[&str])] = &[];

pub fn command_names() -> impl Iterator<Item = &'static str> {
    CANDIDATES.iter().map(|(program, _)| *program)
}

pub fn copy(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("nothing selected".into());
    }
    let mut failures = Vec::new();
    for (program, args) in CANDIDATES {
        match copy_with(program, args, text) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    if failures.is_empty() {
        Err(format!(
            "no clipboard command found (tried {})",
            command_names().collect::<Vec<_>>().join(", ")
        ))
    } else {
        Err(format!("clipboard copy failed: {}", failures.join("; ")))
    }
}

fn copy_with(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("clipboard stdin unavailable"))?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_platform_has_a_clipboard_candidate() {
        assert!(command_names().next().is_some());
    }
}
