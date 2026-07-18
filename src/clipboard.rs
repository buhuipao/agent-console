use std::{
    io::Write,
    process::{Command, Stdio},
};

pub fn copy(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("nothing selected".into());
    }
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start pbcopy: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "pbcopy stdin unavailable".to_owned())?
        .write_all(text.as_bytes())
        .map_err(|error| format!("cannot copy text: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for pbcopy: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pbcopy exited with {status}"))
    }
}
