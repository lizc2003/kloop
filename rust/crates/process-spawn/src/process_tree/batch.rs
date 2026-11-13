//! Windows batch-file launch rules.
//!
//! `CreateProcessW` does not run a `.bat`/`.cmd` file itself: Windows hands it
//! to `cmd.exe`, whose command line has a second parsing pass on top of the
//! usual C argv rules — `%VAR%` expands, `^` escapes, and quotes are handled
//! differently. The standard library keeps the escaping rules of that second
//! pass (`make_bat_command_line`/`append_bat_arg` in
//! `library/std/src/sys/args/windows.rs`, where it also documents the
//! BatBadBut class of injections). We build the same command line ourselves
//! instead of handing the batch path to `CreateProcessW` so the process-tree
//! spawner can still own the child.
//!
//! Everything here works on UTF-16 code units and is compiled on every host,
//! so the escaping rules stay unit-testable away from Windows.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io;
use std::path::Path;

/// `/e:ON` enables the command extensions the `%` escape depends on.
const CMD_PREFIX: &str = "cmd.exe /e:ON /v:OFF /d /c \"";

/// ASCII characters that are safe unquoted inside a batch argument.
const UNQUOTED: &str = r"#$*+-./:?@\_";

/// `%` would start environment-variable expansion; this expands to nothing and
/// consumes the `%` that would otherwise have started `%VAR%`.
const PERCENT_ESCAPE: [u16; 7] = [
    b'%' as u16,
    b'%' as u16,
    b'c' as u16,
    b'd' as u16,
    b':' as u16,
    b'~' as u16,
    b',' as u16,
];

pub(crate) fn is_batch_script(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        })
}

/// The full command line (NUL terminated) that runs `script` through `cmd.exe`
/// with arguments a batch file can reconstruct.
pub(crate) fn command_line(script: &[u16], args: &[Vec<u16>]) -> io::Result<Vec<u16>> {
    const INVALID_SCRIPT: &str = "batch script path contains `\"`, `^` or `%`, or ends with `\\`";
    if script
        .iter()
        .any(|unit| matches!(*unit, u if u == b'"' as u16 || u == b'^' as u16 || u == b'%' as u16))
        || script.last() == Some(&(b'\\' as u16))
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, INVALID_SCRIPT));
    }
    let mut cmd: Vec<u16> = CMD_PREFIX.encode_utf16().collect();
    // The script name gets its own quote pair inside the outer one, so cmd
    // receives exactly one argument for it.
    cmd.push(b'"' as u16);
    cmd.extend_from_slice(script);
    cmd.push(b'"' as u16);
    for arg in args {
        if arg
            .iter()
            .any(|unit| *unit == b'\r' as u16 || *unit == b'\n' as u16)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch file arguments may not contain CR or LF",
            ));
        }
        cmd.push(b' ' as u16);
        append_arg(&mut cmd, arg)?;
    }
    // Close the outer quote opened by CMD_PREFIX.
    cmd.push(b'"' as u16);
    cmd.push(0);
    Ok(cmd)
}

fn append_arg(cmd: &mut Vec<u16>, arg: &[u16]) -> io::Result<()> {
    if arg.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch file arguments may not contain NUL",
        ));
    }
    // Empty arguments and a trailing `\` must be quoted: an unquoted trailing
    // backslash would escape the closing quote when the script re-quotes it.
    let mut quote = arg.is_empty() || arg.last() == Some(&(b'\\' as u16));
    if !quote && arg.iter().any(|unit| unit_needs_quotes(*unit)) {
        quote = true;
    }
    if quote {
        cmd.push(b'"' as u16);
    }
    let mut backslashes = 0usize;
    for unit in arg {
        if *unit == b'\\' as u16 {
            backslashes += 1;
        } else {
            if *unit == b'"' as u16 {
                // n backslashes before an internal quote become 2n.
                cmd.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                // Doubling the quote escapes it for cmd.
                cmd.push(b'"' as u16);
            } else if *unit == b'%' as u16 {
                cmd.extend_from_slice(&PERCENT_ESCAPE);
            }
            backslashes = 0;
        }
        cmd.push(*unit);
    }
    if quote {
        // n trailing backslashes become 2n before the closing quote.
        cmd.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        cmd.push(b'"' as u16);
    }
    Ok(())
}

fn unit_needs_quotes(unit: u16) -> bool {
    if unit < 0x80 {
        let character = unit as u8 as char;
        !(character.is_ascii_alphanumeric() || UNQUOTED.contains(character))
    } else {
        // Unicode control characters (C1); surrogate code units are not
        // control characters and need no quotes.
        (0x7f..=0x9f).contains(&unit)
    }
}

/// Windows glue: the script path and arguments as UTF-16 code units.
#[cfg(windows)]
pub(crate) fn wide_units(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    value.encode_wide().collect()
}

#[cfg(windows)]
pub(crate) fn wide_args(args: &[std::ffi::OsString]) -> Vec<Vec<u16>> {
    args.iter().map(|arg| wide_units(arg.as_os_str())).collect()
}

/// The interpreter for batch scripts. `SystemRoot` is where every Windows
/// install keeps it; the child environment may override `COMSPEC`, so it is
/// not used here.
#[cfg(windows)]
pub(crate) fn command_prompt() -> io::Result<std::path::PathBuf> {
    let root = std::env::var_os("SystemRoot").ok_or_else(|| {
        io::Error::other("SystemRoot is not set; cannot locate the batch interpreter")
    })?;
    Ok(Path::new(&root).join("System32").join("cmd.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(script: &str, args: &[&str]) -> String {
        let script: Vec<u16> = script.encode_utf16().collect();
        let args: Vec<Vec<u16>> = args
            .iter()
            .map(|arg| arg.encode_utf16().collect())
            .collect();
        let line = command_line(&script, &args).unwrap();
        let end = line.iter().position(|unit| *unit == 0).unwrap();
        String::from_utf16(&line[..end]).unwrap()
    }

    #[test]
    fn plain_arguments_are_passed_through() {
        assert_eq!(
            render(r"C:\tools\server.cmd", &["--port", "8080"]),
            "cmd.exe /e:ON /v:OFF /d /c \"\"C:\\tools\\server.cmd\" --port 8080\""
        );
    }

    #[test]
    fn spaces_and_quotes_are_escaped_for_cmd() {
        assert_eq!(
            render("a.cmd", &["hello world", "say \"hi\""]),
            "cmd.exe /e:ON /v:OFF /d /c \"\"a.cmd\" \"hello world\" \"say \"\"hi\"\"\"\""
        );
    }

    /// A trailing backslash would escape the closing quote a script adds when
    /// it re-quotes `%~1`, so the argument is quoted and the backslash doubled.
    #[test]
    fn trailing_backslashes_are_doubled_inside_quotes() {
        assert_eq!(
            render("a.cmd", &[r"C:\dir\"]),
            "cmd.exe /e:ON /v:OFF /d /c \"\"a.cmd\" \"C:\\dir\\\\\"\""
        );
    }

    /// `%VAR%` would expand; the no-op `%%cd:~,%` consumes the `%` instead.
    #[test]
    fn percent_signs_do_not_expand_environment_variables() {
        assert_eq!(
            render("a.cmd", &["100%"]),
            "cmd.exe /e:ON /v:OFF /d /c \"\"a.cmd\" \"100%%cd:~,%\"\""
        );
    }

    #[test]
    fn a_script_path_cmd_cannot_quote_is_rejected() {
        for script in ["50%.cmd", "a^b.cmd", "a\"b.cmd", "dir\\"] {
            let units: Vec<u16> = script.encode_utf16().collect();
            assert!(
                command_line(&units, &[]).is_err(),
                "script {script} must be rejected"
            );
        }
    }

    #[test]
    fn arguments_with_line_breaks_or_nul_are_rejected() {
        for arg in ["two\nlines", "two\rlines"] {
            let units: Vec<u16> = arg.encode_utf16().collect();
            assert!(command_line(&[b'a' as u16], &[units]).is_err());
        }
        assert!(command_line(&[b'a' as u16], &[vec![b'x' as u16, 0]]).is_err());
    }

    #[test]
    fn batch_detection_is_case_insensitive() {
        assert!(is_batch_script(Path::new("npx.CMD")));
        assert!(is_batch_script(Path::new("setup.Bat")));
        assert!(!is_batch_script(Path::new("npx.exe")));
        assert!(!is_batch_script(Path::new("npx")));
    }
}
