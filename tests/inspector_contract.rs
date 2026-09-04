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
            "innards-inspector-{test_name}-{}-{nonce}",
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

fn assert_terminal_mode_restored(before_path: &Path, after_path: &Path) {
    let before = fs::read_to_string(before_path).expect("read terminal mode before run");
    let after = fs::read_to_string(after_path).expect("read terminal mode after run");
    #[cfg(target_os = "macos")]
    assert_eq!(
        normalize_macos_terminal_mode(&before),
        normalize_macos_terminal_mode(&after)
    );
    #[cfg(not(target_os = "macos"))]
    assert_eq!(before, after);
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
            format!("lflag={:x}", flags & !libc::PENDIN)
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn fixture(test_name: &str) -> (ScratchDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let scratch = ScratchDir::new(test_name);
    let input = scratch.join("inspection.json");
    let result = scratch.join("result.json");
    let before = scratch.join("before.stty");
    let after = scratch.join("after.stty");
    fs::write(
        &input,
        r#"{"schema_version":1,"object_id":"counter_123","class_name":"Counter","data":{"value":42}}"#,
    )
    .expect("write inspection input");
    (scratch, input, result, before, after)
}

#[test]
fn inspector_returns_typed_leaf_proposal_and_restores_terminal() {
    let (_scratch, input, result, before, after) = fixture("proposal");
    let binary = Path::new(env!("CARGO_BIN_EXE_ininspect"));
    let script = r#"
        log_user 1
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set input $env(INNARDS_TEST_ARG_1)
        set result $env(INNARDS_TEST_ARG_2)
        set before $env(INNARDS_TEST_ARG_3)
        set after $env(INNARDS_TEST_ARG_4)
        set command [format {stty rows 24 columns 100; stty -g </dev/tty > %s; %s --result-json < %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before $binary $input $result $after]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "\033\[B"
        send -- "e"
        send -- "\025"
        send -- "43"
        send -- "\r"
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

    let output = run_expect(script, &[binary, &input, &result, &before, &after]);
    assert!(
        output.status.success(),
        "expect failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let terminal = String::from_utf8_lossy(&output.stdout);
    assert!(
        terminal.contains("value"),
        "tree did not render value: {terminal}"
    );
    let text = fs::read_to_string(&result).expect("read result");
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains('\u{1b}'));
    let value: serde_json::Value = serde_json::from_str(&text).expect("parse result");
    assert_eq!(value["outcome"], "proposed");
    assert_eq!(value["object_id"], "counter_123");
    assert_eq!(value["base_data"], serde_json::json!({"value": 42}));
    assert_eq!(value["proposal"]["path"], serde_json::json!(["value"]));
    assert_eq!(value["proposal"]["old_value"], 42);
    assert_eq!(value["proposal"]["new_value"], 43);
    assert_terminal_mode_restored(&before, &after);
}

#[test]
fn inspector_view_exit_is_structured_and_non_mutating() {
    let (_scratch, input, result, before, after) = fixture("viewed");
    let binary = Path::new(env!("CARGO_BIN_EXE_ininspect"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set input $env(INNARDS_TEST_ARG_1)
        set result $env(INNARDS_TEST_ARG_2)
        set before $env(INNARDS_TEST_ARG_3)
        set after $env(INNARDS_TEST_ARG_4)
        set command [format {stty -g </dev/tty > %s; %s --result-json < %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before $binary $input $result $after]
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

    let output = run_expect(script, &[binary, &input, &result, &before, &after]);
    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&result).unwrap()).unwrap();
    assert_eq!(value["outcome"], "viewed");
    assert!(value["proposal"].is_null());
    assert!(value["base_data"].is_null());
    assert_terminal_mode_restored(&before, &after);
}
