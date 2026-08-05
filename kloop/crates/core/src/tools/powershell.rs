use std::path::Path;

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;

use super::bash;
use super::str_arg;
use super::ToolCtx;
use crate::process_tree::ProcessSpec;
use crate::shell_programs::ShellFlavor;
use crate::shell_programs::ShellProgram;

const DEFAULT_TIMEOUT_MS: u64 = 60_000;

pub(super) async fn powershell_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    validate_input(input)?;
    let command = str_arg(input, "command", "powershell")?;
    let timeout_ms = timeout_ms(input)?;
    let program = ctx
        .cfg
        .shell_programs
        .powershell
        .as_ref()
        .context("powershell: no trusted PowerShell executable is available in this session")?;
    if !matches!(
        program.flavor,
        ShellFlavor::PowerShell7 | ShellFlavor::WindowsPowerShell
    ) {
        bail!("powershell: resolved executable has the wrong shell flavor");
    }
    if ctx.cancel.is_cancelled() {
        bail!("interrupted");
    }

    let spec = powershell_spec(program, command, &ctx.cfg.effective_cwd());
    let output = bash::run_process_foreground(spec, timeout_ms, &ctx.cancel, "powershell").await?;
    Ok(bash::format_output(&output))
}

fn powershell_spec(program: &ShellProgram, command: &str, cwd: &Path) -> ProcessSpec {
    let payload = encoded_payload(command);
    let mut spec = ProcessSpec::new(&program.executable, cwd);
    #[cfg(windows)]
    spec.require_windows_descendant_debugging();
    for arg in [
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-EncodedCommand",
    ] {
        spec.arg(arg);
    }
    spec.arg(encode_command(&payload));
    bash::scrub_model_shell_env(&mut spec);
    spec
}

fn timeout_ms(input: &Value) -> Result<u64> {
    match input.get("timeout_ms") {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(value) => value
            .as_u64()
            .filter(|timeout| *timeout > 0)
            .context("powershell: timeout_ms must be a positive integer"),
    }
}

fn validate_input(input: &Value) -> Result<()> {
    let object = input
        .as_object()
        .context("powershell: input must be an object")?;
    for key in object.keys() {
        if !matches!(key.as_str(), "command" | "timeout_ms") {
            bail!(
                "powershell: unsupported field '{key}'; PowerShell v1 is foreground-only and does not support background, sandbox, stdin, PTY, session or executable overrides"
            );
        }
    }
    Ok(())
}

fn encoded_payload(command: &str) -> String {
    format!(
        "$__kloopUtf8 = [System.Text.UTF8Encoding]::new($false)\n\
         try {{ [Console]::InputEncoding = $__kloopUtf8 }} catch {{}}\n\
         try {{ [Console]::OutputEncoding = $__kloopUtf8 }} catch {{}}\n\
         $OutputEncoding = $__kloopUtf8\n\
         $__kloopErrorCountBefore = $Error.Count\n\
         $LASTEXITCODE = $null\n\
         $__kloopPowerShellSucceeded = $true\n\
         $__kloopNativeExitCode = $null\n\
         & {{\n{command}\n\
         $script:__kloopPowerShellSucceeded = $?\n\
         $script:__kloopNativeExitCode = $LASTEXITCODE\n\
         }}\n\
         $__kloopHadNewError = $Error.Count -gt $__kloopErrorCountBefore\n\
         if ($__kloopHadNewError) {{ exit 1 }}\n\
         if ($__kloopPowerShellSucceeded) {{ exit 0 }}\n\
         if ($null -ne $__kloopNativeExitCode -and $__kloopNativeExitCode -ne 0) {{ exit [int]$__kloopNativeExitCode }}\n\
         exit 1\n"
    )
}

fn encode_command(payload: &str) -> String {
    let mut bytes = Vec::with_capacity(payload.len() * 2);
    for unit in payload.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_command_is_utf16le_without_bom_and_keeps_original_script() {
        let command = "Write-Output '你好'\n# trailing comment";
        let payload = encoded_payload(command);
        let encoded = encode_command(&payload);
        let bytes = STANDARD.decode(encoded).unwrap();
        assert!(!bytes.starts_with(&[0xff, 0xfe]));
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let decoded = String::from_utf16(&units).unwrap();
        assert_eq!(decoded, payload);
        assert!(decoded.contains(
            "Write-Output '你好'\n# trailing comment\n$script:__kloopPowerShellSucceeded = $?"
        ));
        assert!(decoded.contains("$LASTEXITCODE"));
        assert!(decoded.contains("[System.Text.UTF8Encoding]::new($false)"));
    }

    #[test]
    fn wrapper_puts_exit_snapshot_after_a_fresh_line() {
        let payload = encoded_payload("native.exe # keep comment");
        assert!(
            payload.contains("native.exe # keep comment\n$script:__kloopPowerShellSucceeded = $?")
        );
    }

    #[test]
    fn wrapper_uses_final_powershell_status_before_native_exit_code() {
        let payload = encoded_payload("Write-Output ok");
        let reset = payload.find("$LASTEXITCODE = $null").unwrap();
        let command = payload.find("Write-Output ok").unwrap();
        let snapshot = payload
            .find("$script:__kloopPowerShellSucceeded = $?")
            .unwrap();
        let error = payload.find("if ($__kloopHadNewError) { exit 1 }").unwrap();
        let success = payload
            .find("if ($__kloopPowerShellSucceeded) { exit 0 }")
            .unwrap();
        let native = payload
            .find("if ($null -ne $__kloopNativeExitCode")
            .unwrap();
        assert!(reset < command);
        assert!(command < snapshot && snapshot < error);
        assert!(error < success && success < native);
    }

    #[test]
    fn process_spec_uses_only_the_fixed_noninteractive_encoded_argv() {
        let program = ShellProgram {
            executable: std::path::PathBuf::from("/trusted/pwsh.exe"),
            flavor: ShellFlavor::PowerShell7,
        };
        let cwd = std::path::Path::new("/workspace");
        let spec = powershell_spec(&program, "Write-Output ok", cwd);
        assert_eq!(spec.executable, program.executable);
        assert_eq!(spec.cwd, cwd);
        assert_eq!(
            spec.args[..4],
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand"
            ]
            .map(std::ffi::OsString::from)
        );
        assert_eq!(spec.args.len(), 5);
        assert!(!spec
            .args
            .iter()
            .any(|arg| arg.to_string_lossy().contains("ExecutionPolicy")));
    }

    #[test]
    fn input_rejects_every_non_v1_field() {
        for field in [
            "run_in_background",
            "background",
            "disable_sandbox",
            "stdin",
            "tty",
            "session",
            "executable",
        ] {
            let mut input = serde_json::json!({"command": "Get-Location"});
            input[field] = serde_json::json!(true);
            let error = validate_input(&input).unwrap_err().to_string();
            assert!(error.contains(field), "{error}");
            assert!(error.contains("foreground-only"), "{error}");
        }
    }

    #[test]
    fn timeout_must_be_positive_integer() {
        assert_eq!(
            timeout_ms(&serde_json::json!({"command": "Get-Location"})).unwrap(),
            60_000
        );
        for value in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("10"),
        ] {
            let input = serde_json::json!({"command": "Get-Location", "timeout_ms": value});
            let error = timeout_ms(&input).unwrap_err().to_string();
            assert!(error.contains("positive integer"), "{error}");
        }
    }

    #[cfg(windows)]
    mod native_windows {
        use super::*;

        use tokio_util::sync::CancellationToken;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

        fn installed_programs() -> Vec<ShellProgram> {
            let (programs, _) = crate::shell_programs::resolve_shell_programs(Default::default())
                .expect("native Windows shell discovery succeeds");
            let powershell7 = programs
                .powershell
                .expect("a trusted PowerShell 7 installation is required");
            assert_eq!(
                powershell7.flavor,
                ShellFlavor::PowerShell7,
                "native tests require PowerShell 7 in addition to Windows PowerShell 5.1"
            );
            let system_root = std::env::var_os("SystemRoot")
                .map(std::path::PathBuf::from)
                .expect("SystemRoot is present on native Windows");
            let programs = vec![
                powershell7,
                ShellProgram {
                    executable: system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                    flavor: ShellFlavor::WindowsPowerShell,
                },
            ];
            for program in &programs {
                assert!(
                    program.executable.is_file(),
                    "required native Windows fixture is missing: {}",
                    program.executable.display()
                );
            }
            programs
        }

        async fn run(program: &ShellProgram, command: &str, timeout_ms: u64) -> Result<String> {
            let spec = powershell_spec(program, command, &std::env::current_dir()?);
            let cancel = CancellationToken::new();
            let output =
                bash::run_process_foreground(spec, timeout_ms, &cancel, "powershell").await?;
            Ok(bash::format_output(&output))
        }

        fn quoted_literal(path: &Path) -> String {
            format!(
                "'{}'",
                path.to_string_lossy()
                    .replace('\\', "/")
                    .replace('\'', "''")
            )
        }

        fn process_has_exited(pid: u32) -> bool {
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
            if handle == 0 {
                return true;
            }
            let exited = unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
            unsafe {
                CloseHandle(handle);
            }
            exited
        }

        fn descendant_script(marker: &Path) -> String {
            format!(
                "$child = Start-Process -FilePath $env:ComSpec -ArgumentList '/D','/C','ping 127.0.0.1 -n 30 > NUL' -PassThru\n\
                 [IO.File]::WriteAllText({}, [string]$child.Id)\n\
                 Start-Sleep -Seconds 30",
                quoted_literal(marker)
            )
        }

        async fn wait_for_pid(marker: &Path) -> u32 {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(marker) {
                        if let Ok(pid) = text.trim().parse() {
                            return pid;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{} was not written", marker.display()))
        }

        #[tokio::test]
        async fn both_flavors_preserve_script_text_and_exit_semantics() {
            let script = "Write-Output '你好'\n\
                          Write-Output \"double`\"quote\"\n\
                          @'\n\
                          here ' single and \" double\n\
                          '@\n\
                          Write-Output 'tail' # trailing comment";
            for program in installed_programs() {
                let label = format!("{:?}", program.flavor);
                let output = run(&program, script, 10_000).await.unwrap();
                assert!(output.contains("你好"), "{label}: {output}");
                assert!(output.contains("double\"quote"), "{label}: {output}");
                assert!(
                    output.contains("here ' single and \" double"),
                    "{label}: {output}"
                );
                assert!(output.contains("tail"), "{label}: {output}");

                let output = run(&program, "exit 7", 10_000).await.unwrap();
                assert!(output.contains("[exit status 7]"), "{label}: {output}");

                let output = run(&program, "& $env:ComSpec /D /C 'exit 9'", 10_000)
                    .await
                    .unwrap();
                assert!(output.contains("[exit status 9]"), "{label}: {output}");

                let output = run(&program, "& $env:ComSpec /D /C 'exit 9'; $null", 10_000)
                    .await
                    .unwrap();
                assert_eq!(output, "(no output)", "{label}");

                let output = run(
                    &program,
                    "& $env:ComSpec /D /C 'exit 9'; Write-Output ok",
                    10_000,
                )
                .await
                .unwrap();
                assert!(
                    output.lines().any(|line| line.trim_end() == "ok"),
                    "{label}: {output}"
                );
                assert!(!output.contains("[exit status 9]"), "{label}: {output}");

                let output = run(
                    &program,
                    "& $env:ComSpec /D /C 'exit 9'; Write-Error 'later-powershell-error' -ErrorAction Continue",
                    10_000,
                )
                .await
                .unwrap();
                assert!(
                    output.contains("later-powershell-error"),
                    "{label}: {output}"
                );
                assert!(output.contains("[exit status 1]"), "{label}: {output}");
                assert!(!output.contains("[exit status 9]"), "{label}: {output}");

                let output = run(
                    &program,
                    "Write-Error 'kloop-nonterm' -ErrorAction Continue",
                    10_000,
                )
                .await
                .unwrap();
                assert!(output.contains("kloop-nonterm"), "{label}: {output}");
                assert!(output.contains("[exit status 1]"), "{label}: {output}");

                let output = run(&program, "throw 'kloop-terminating'", 10_000)
                    .await
                    .unwrap();
                assert!(output.contains("kloop-terminating"), "{label}: {output}");
                assert!(output.contains("[exit status 1]"), "{label}: {output}");

                assert_eq!(run(&program, "$null", 10_000).await.unwrap(), "(no output)");
                let read_host = run(&program, "Read-Host 'prompt' | Out-Null", 5_000)
                    .await
                    .unwrap_or_else(|error| panic!("{label}: {error:#}"));
                assert!(!read_host.contains("timed out"), "{label}: {read_host}");
            }
        }

        #[tokio::test]
        async fn both_flavors_drain_and_bound_large_dual_streams() {
            for program in installed_programs() {
                let label = format!("{:?}", program.flavor);
                let output = run(
                    &program,
                    "[Console]::Out.Write(('O' * 200000)); [Console]::Error.Write(('E' * 200000))",
                    20_000,
                )
                .await
                .unwrap();
                assert!(output.contains("[output truncated:"), "{label}: {output}");
                assert!(output.len() < 40_000, "{label}: {} bytes", output.len());
            }
        }

        #[tokio::test]
        async fn timeout_kills_start_process_descendants_for_both_flavors() {
            let dir = std::env::temp_dir().join(format!(
                "kloop-powershell-descendants-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for program in installed_programs() {
                let marker = dir.join(format!("{:?}.pid", program.flavor));
                let script = descendant_script(&marker);
                let error = run(&program, &script, 5_000).await.unwrap_err();
                assert!(error.to_string().contains("timed out"), "{error:#}");
                let pid = wait_for_pid(&marker).await;
                assert!(
                    process_has_exited(pid),
                    "{:?} left descendant {pid} alive",
                    program.flavor
                );
            }
            let _ = std::fs::remove_dir_all(dir);
        }

        #[tokio::test]
        async fn cancellation_kills_start_process_descendants_for_both_flavors() {
            let dir = std::env::temp_dir().join(format!(
                "kloop-powershell-cancel-descendants-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for program in installed_programs() {
                let marker = dir.join(format!("{:?}.pid", program.flavor));
                let spec = powershell_spec(
                    &program,
                    &descendant_script(&marker),
                    &std::env::current_dir().unwrap(),
                );
                let cancel = CancellationToken::new();
                let execution = bash::run_process_foreground(spec, 60_000, &cancel, "powershell");
                let cancellation = async {
                    let pid = wait_for_pid(&marker).await;
                    cancel.cancel();
                    pid
                };
                let (result, pid) = tokio::join!(execution, cancellation);
                let error = result.unwrap_err();
                assert_eq!(error.to_string(), "interrupted");
                assert!(
                    process_has_exited(pid),
                    "{:?} left cancelled descendant {pid} alive",
                    program.flavor
                );
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
