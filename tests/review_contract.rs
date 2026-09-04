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
            "innards-review-{test_name}-{}-{nonce}",
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
            format!("lflag={:x}", flags & !(libc::PENDIN as u64))
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn fixture(test_name: &str) -> (ScratchDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let scratch = ScratchDir::new(test_name);
    let diff = scratch.join("proposal.diff");
    let result = scratch.join("result.json");
    let before = scratch.join("before.stty");
    let after = scratch.join("after.stty");
    fs::write(
        &diff,
        "--- a/trash/Counter.trash\n+++ b/trash/Counter.trash\n@@ -1 +1 @@\n-old value\n+new value\n@@ -10 +10 @@\n-old test\n+new test\n",
    )
    .expect("write diff fixture");
    (scratch, diff, result, before, after)
}

fn parse_single_json_line(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path).expect("read result");
    assert_eq!(text.lines().count(), 1, "result should be one JSON line");
    assert!(
        !text.contains('\u{1b}'),
        "result must not contain terminal output"
    );
    serde_json::from_str(&text).expect("parse result")
}

#[test]
fn accepts_without_mutating_input_and_restores_terminal() {
    let (_scratch, diff, result, before, after) = fixture("accept");
    let original = fs::read_to_string(&diff).unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_indiff"));
    let script = r#"
        log_user 1
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set diff $env(INNARDS_TEST_ARG_1)
        set result $env(INNARDS_TEST_ARG_2)
        set before $env(INNARDS_TEST_ARG_3)
        set after $env(INNARDS_TEST_ARG_4)
        set command [format {stty rows 24 columns 100; stty -g </dev/tty > %s; %s --result-json < %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before $binary $diff $result $after]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "ra"
        expect {
            eof {}
            timeout { exit 97 }
        }
        set result [wait]
        exit [lindex $result 3]
    "#;
    let output = run_expect(script, &[binary, &diff, &result, &before, &after]);
    assert!(
        output.status.success(),
        "expect failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("+new value"));
    let decision = parse_single_json_line(&result);
    assert_eq!(decision["outcome"], "accepted");
    assert_eq!(decision["accepted_hunks"], serde_json::json!([1]));
    assert_eq!(decision["rejected_hunks"], serde_json::json!([0]));
    assert_eq!(fs::read_to_string(&diff).unwrap(), original);
    assert_terminal_mode_restored(&before, &after);
}

#[test]
fn rejection_is_structured_and_nonzero() {
    let (_scratch, diff, result, before, after) = fixture("reject");
    let binary = Path::new(env!("CARGO_BIN_EXE_indiff"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set diff $env(INNARDS_TEST_ARG_1)
        set result $env(INNARDS_TEST_ARG_2)
        set before $env(INNARDS_TEST_ARG_3)
        set after $env(INNARDS_TEST_ARG_4)
        set command [format {stty -g </dev/tty > %s; %s --result-json < %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before $binary $diff $result $after]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
        send -- "R"
        expect { eof {} timeout { exit 97 } }
        set result [wait]
        exit [lindex $result 3]
    "#;
    let output = run_expect(script, &[binary, &diff, &result, &before, &after]);
    assert_eq!(output.status.code(), Some(3));
    let decision = parse_single_json_line(&result);
    assert_eq!(decision["outcome"], "rejected");
    assert_eq!(decision["accepted_hunks"], serde_json::json!([]));
    assert_eq!(decision["rejected_hunks"], serde_json::json!([0, 1]));
    assert_terminal_mode_restored(&before, &after);
}
