#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(test_name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "innards-{test_name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create scratch directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_expect(script: &str, args: &[&Path]) -> Output {
    let mut command = Command::new("/usr/bin/expect");
    command.arg("-c").arg(script);
    for (index, arg) in args.iter().enumerate() {
        command.env(format!("INNARDS_TEST_ARG_{index}"), arg);
    }
    command.output().expect("run expect")
}

fn parse_single_json_line(path: &Path) -> serde_json::Value {
    let output = fs::read_to_string(path).expect("read result JSON");
    assert_eq!(output.lines().count(), 1, "stdout must contain one record");
    assert!(
        !output.contains('\u{1b}'),
        "stdout must not contain terminal escapes"
    );
    serde_json::from_str(&output).expect("result should be valid JSON")
}

fn assert_terminal_mode_restored(before_path: &Path, after_path: &Path) {
    let before = fs::read_to_string(before_path).expect("read terminal mode before run");
    let after = fs::read_to_string(after_path).expect("read terminal mode after run");

    #[cfg(target_os = "macos")]
    assert_eq!(
        normalize_macos_terminal_mode(&before),
        normalize_macos_terminal_mode(&after),
        "terminal mode was not restored"
    );
    #[cfg(not(target_os = "macos"))]
    assert_eq!(before, after, "terminal mode was not restored");
}

#[cfg(target_os = "macos")]
fn normalize_macos_terminal_mode(mode: &str) -> String {
    mode.trim()
        .split(':')
        .map(|field| {
            let Some(flags) = field.strip_prefix("lflag=") else {
                return field.to_string();
            };
            let flags = u64::from_str_radix(flags, 16).expect("lflag should be hexadecimal");
            format!("lflag={:x}", flags & !(libc::PENDIN as u64))
        })
        .collect::<Vec<_>>()
        .join(":")
}

#[test]
fn piped_input_uses_tty_for_ui_and_stdout_for_one_result() {
    let scratch = ScratchDir::new("piped-input");
    let result_path = scratch.join("result.json");
    let before_path = scratch.join("before.stty");
    let after_path = scratch.join("after.stty");
    let binary = Path::new(env!("CARGO_BIN_EXE_inpage"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set result_path $env(INNARDS_TEST_ARG_1)
        set before_path $env(INNARDS_TEST_ARG_2)
        set after_path $env(INNARDS_TEST_ARG_3)
        set command [format {stty -g </dev/tty > %s; printf 'alpha\nbeta\n' | %s --stdin --result-json > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before_path $binary $result_path $after_path]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "q"
        expect {
            eof {}
            timeout {
                set pid [exp_pid]
                catch {exec /bin/kill -KILL -- -$pid}
                catch {expect eof}
                exit 97
            }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;

    let output = run_expect(script, &[binary, &result_path, &before_path, &after_path]);
    assert!(
        output.status.success(),
        "expect failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = parse_single_json_line(&result_path);
    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["outcome"], "closed");
    assert_eq!(result["path"], serde_json::Value::Null);
    assert_eq!(result["changed"], false);
    assert_terminal_mode_restored(&before_path, &after_path);
}

#[test]
fn ctrl_c_cancels_and_restores_terminal_mode() {
    let scratch = ScratchDir::new("ctrl-c");
    let result_path = scratch.join("result.json");
    let before_path = scratch.join("before.stty");
    let after_path = scratch.join("after.stty");
    let binary = Path::new(env!("CARGO_BIN_EXE_inpage"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set result_path $env(INNARDS_TEST_ARG_1)
        set before_path $env(INNARDS_TEST_ARG_2)
        set after_path $env(INNARDS_TEST_ARG_3)
        set command [format {stty -g </dev/tty > %s; printf 'alpha\n' | %s --stdin --result-json > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before_path $binary $result_path $after_path]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "\003"
        expect {
            eof {}
            timeout {
                set pid [exp_pid]
                catch {exec /bin/kill -KILL -- -$pid}
                catch {expect eof}
                exit 97
            }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;

    let output = run_expect(script, &[binary, &result_path, &before_path, &after_path]);
    assert_eq!(
        output.status.code(),
        Some(130),
        "unexpected status: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = parse_single_json_line(&result_path);
    assert_eq!(result["outcome"], "cancelled");
    assert_terminal_mode_restored(&before_path, &after_path);
}

#[test]
fn dirty_editor_exit_reports_discarded_without_writing_the_file() {
    let scratch = ScratchDir::new("dirty-editor");
    let source_path = scratch.join("source.txt");
    let result_path = scratch.join("result.json");
    let before_path = scratch.join("before.stty");
    let after_path = scratch.join("after.stty");
    fs::write(&source_path, "alpha\n").expect("write source");
    let binary = Path::new(env!("CARGO_BIN_EXE_inmacs"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set source_path $env(INNARDS_TEST_ARG_1)
        set result_path $env(INNARDS_TEST_ARG_2)
        set before_path $env(INNARDS_TEST_ARG_3)
        set after_path $env(INNARDS_TEST_ARG_4)
        set command [format {stty -g </dev/tty > %s; %s --result-json %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before_path $binary $source_path $result_path $after_path]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "x\030\003"
        expect {
            eof {}
            timeout {
                set pid [exp_pid]
                catch {exec /bin/kill -KILL -- -$pid}
                catch {expect eof}
                exit 97
            }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;

    let output = run_expect(
        script,
        &[
            binary,
            &source_path,
            &result_path,
            &before_path,
            &after_path,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected status: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = parse_single_json_line(&result_path);
    assert_eq!(result["outcome"], "discarded");
    assert_eq!(result["changed"], true);
    assert_eq!(result["edit_count"], 1);
    assert_eq!(fs::read_to_string(&source_path).unwrap(), "alpha\n");
    assert_terminal_mode_restored(&before_path, &after_path);
}

#[test]
fn stdin_editor_saves_to_explicit_output() {
    let scratch = ScratchDir::new("stdin-editor");
    let result_path = scratch.join("result.json");
    let output_path = scratch.join("output.trash");
    let before_path = scratch.join("before.stty");
    let after_path = scratch.join("after.stty");
    let binary = Path::new(env!("CARGO_BIN_EXE_inmacs"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set result_path $env(INNARDS_TEST_ARG_1)
        set output_path $env(INNARDS_TEST_ARG_2)
        set before_path $env(INNARDS_TEST_ARG_3)
        set after_path $env(INNARDS_TEST_ARG_4)
        set command [format {stty -g </dev/tty > %s; printf 'alpha\n' | %s --stdin --output %s --result-json > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before_path $binary $output_path $result_path $after_path]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "x\030\023\030\003"
        expect {
            eof {}
            timeout {
                set pid [exp_pid]
                catch {exec /bin/kill -KILL -- -$pid}
                catch {expect eof}
                exit 97
            }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;

    let output = run_expect(
        script,
        &[
            binary,
            &result_path,
            &output_path,
            &before_path,
            &after_path,
        ],
    );
    assert!(
        output.status.success(),
        "unexpected status: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = parse_single_json_line(&result_path);
    assert_eq!(result["outcome"], "saved");
    assert_eq!(result["changed"], true);
    assert_eq!(fs::read_to_string(&output_path).unwrap(), "xalpha\n");
    assert_terminal_mode_restored(&before_path, &after_path);
}

#[test]
fn annotations_render_without_changing_source() {
    let scratch = ScratchDir::new("annotations");
    let source_path = scratch.join("source.trash");
    let annotations_path = scratch.join("annotations.json");
    let result_path = scratch.join("result.json");
    fs::write(&source_path, "Example subclass: Object\n").unwrap();
    fs::write(
        &annotations_path,
        r#"{"schema_version":1,"annotations":[{"line":1,"column":9,"severity":"error","message":"compile: expected ]"}]}"#,
    )
    .unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_inpage"));
    let script = r#"
        log_user 1
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set source_path $env(INNARDS_TEST_ARG_1)
        set annotations_path $env(INNARDS_TEST_ARG_2)
        set result_path $env(INNARDS_TEST_ARG_3)
        set command [format {stty rows 24 columns 80 </dev/tty; exec %s --annotations %s --result-json %s > %s} $binary $annotations_path $source_path $result_path]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "q"
        expect {
            eof {}
            timeout {
                set pid [exp_pid]
                catch {exec /bin/kill -KILL -- -$pid}
                catch {expect eof}
                exit 97
            }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;

    let output = run_expect(
        script,
        &[binary, &source_path, &annotations_path, &result_path],
    );
    assert!(
        output.status.success(),
        "unexpected status: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("compile: expected ]"),
        "annotation was not rendered: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(parse_single_json_line(&result_path)["outcome"], "closed");
    assert_eq!(
        fs::read_to_string(&source_path).unwrap(),
        "Example subclass: Object\n"
    );
}

#[test]
fn termination_signal_produces_a_clean_cancellation_result() {
    let scratch = ScratchDir::new("sigterm");
    let source_path = scratch.join("source.txt");
    let result_path = scratch.join("result.json");
    fs::write(&source_path, "alpha\nbeta\n").expect("write source");
    let binary = Path::new(env!("CARGO_BIN_EXE_inpage"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set source_path $env(INNARDS_TEST_ARG_1)
        set result_path $env(INNARDS_TEST_ARG_2)
        set command [format {exec %s --result-json %s > %s} $binary $source_path $result_path]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        exec kill -TERM [exp_pid]
        expect {
            eof {}
            timeout {
                set pid [exp_pid]
                catch {exec /bin/kill -KILL -- -$pid}
                catch {expect eof}
                exit 97
            }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;

    let output = run_expect(script, &[binary, &source_path, &result_path]);
    assert_eq!(
        output.status.code(),
        Some(130),
        "unexpected status: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = parse_single_json_line(&result_path);
    assert_eq!(result["outcome"], "cancelled");
    assert_eq!(result["path"], source_path.display().to_string());
}
