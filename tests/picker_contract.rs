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
            "innards-picker-{test_name}-{}-{nonce}",
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

fn fixture(test_name: &str) -> (ScratchDir, PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let scratch = ScratchDir::new(test_name);
    let source_path = scratch.join("Pod.trash");
    let candidates_path = scratch.join("candidates.jsonl");
    let result_path = scratch.join("result.json");
    let before_path = scratch.join("before.stty");
    let after_path = scratch.join("after.stty");
    fs::write(
        &source_path,
        "package: Kube\n\nPod subclass: Object\n\n  classMethod: fromJson: json cluster: cluster [\n    ^ json\n  ]\n",
    )
    .expect("write source fixture");
    fs::write(
        &candidates_path,
        format!(
            "{{\"schema_version\":1,\"id\":\"Kube::Pod\",\"path\":\"{}\",\"line\":3,\"column\":1,\"label\":\"Kube::Pod\",\"kind\":\"class\",\"detail\":\"Object\"}}\n{{\"schema_version\":1,\"id\":\"Kube::Pod>>fromJson:cluster:\",\"path\":\"{}\",\"line\":5,\"column\":3,\"label\":\"Kube::Pod>>fromJson:cluster:\",\"kind\":\"class_method\",\"detail\":\"raw=false\"}}\n",
            source_path.display(),
            source_path.display()
        ),
    )
    .expect("write candidates");
    (
        scratch,
        source_path,
        candidates_path,
        result_path,
        before_path,
        after_path,
    )
}

#[test]
fn jsonl_picker_selects_filtered_keyword_method_with_preview_and_clean_stdout() {
    let (_scratch, _source, candidates, result, before, after) = fixture("select");
    let binary = Path::new(env!("CARGO_BIN_EXE_inpick"));
    let script = r#"
        log_user 1
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set candidates $env(INNARDS_TEST_ARG_1)
        set result $env(INNARDS_TEST_ARG_2)
        set before $env(INNARDS_TEST_ARG_3)
        set after $env(INNARDS_TEST_ARG_4)
        set command [format {stty rows 24 columns 100; stty -g </dev/tty > %s; %s --query 'fromJson:cluster:' --result-json < %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before $binary $candidates $result $after]
        spawn -noecho /bin/sh -c $command
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        after 100
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

    let output = run_expect(script, &[binary, &candidates, &result, &before, &after]);
    assert!(
        output.status.success(),
        "expect failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("classMethod: fromJson: json cluster:"),
        "preview did not render source: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = fs::read_to_string(&result).expect("read result");
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains('\u{1b}'));
    let value: serde_json::Value = serde_json::from_str(&text).expect("parse result");
    assert_eq!(value["outcome"], "selected");
    assert_eq!(value["selection"]["id"], "Kube::Pod>>fromJson:cluster:");
    assert_terminal_mode_restored(&before, &after);
}

#[test]
fn jsonl_picker_cancel_is_structured_and_restores_terminal() {
    let (_scratch, _source, candidates, result, before, after) = fixture("cancel");
    let binary = Path::new(env!("CARGO_BIN_EXE_inpick"));
    let script = r#"
        log_user 0
        set timeout 10
        set binary $env(INNARDS_TEST_ARG_0)
        set candidates $env(INNARDS_TEST_ARG_1)
        set result $env(INNARDS_TEST_ARG_2)
        set before $env(INNARDS_TEST_ARG_3)
        set after $env(INNARDS_TEST_ARG_4)
        set command [format {stty -g </dev/tty > %s; %s --result-json < %s > %s; status=$?; stty -g </dev/tty > %s; exit "$status"} $before $binary $candidates $result $after]
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

    let output = run_expect(script, &[binary, &candidates, &result, &before, &after]);
    assert_eq!(output.status.code(), Some(130));
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&result).unwrap()).unwrap();
    assert_eq!(value["outcome"], "cancelled");
    assert!(value["selection"].is_null());
    assert_terminal_mode_restored(&before, &after);
}
