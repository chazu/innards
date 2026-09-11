#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn exercise_resize(binary: &str, surface: &str) -> serde_json::Value {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = ScratchDir(
        std::env::temp_dir().join(format!("innards-resize-{}-{nonce}", std::process::id())),
    );
    fs::create_dir(&scratch.0).unwrap();
    let text = scratch.0.join("source.txt");
    let input = scratch.0.join("input.json");
    let output = scratch.0.join("result.json");
    let before = scratch.0.join("before.stty");
    let after = scratch.0.join("after.stty");
    fs::write(&text, "alpha\nbeta\n").unwrap();
    let contents = match surface {
        "picker" => ["alpha", "beta"].map(|label| serde_json::json!({
            "schema_version":1,"id":label,"path":text,"line":1,"column":1,
            "label":label,"kind":"fixture"
        }).to_string()).join("\n"),
        "inspector" => r#"{"schema_version":1,"object_id":"counter_1","class_name":"Counter","data":{"value":42}}"#.to_owned(),
        "review" => "--- a/source.txt\n+++ b/source.txt\n@@ -1 +1 @@\n-alpha\n+beta\n".to_owned(),
        _ => "alpha\nbeta\n".to_owned(),
    };
    fs::write(&input, &contents).unwrap();

    let script = r#"
        log_user 1
        set timeout 10
        set binary $env(RESIZE_BINARY)
        set surface $env(RESIZE_SURFACE)
        set args "--height 12 --result-json"
        switch $surface {
            editor { append args " " $env(RESIZE_TEXT) }
            pager { append args " --stdin" }
            picker { append args " --query beta" }
        }
        set command [format {stty rows 24 columns 100; stty -g > %s; %s %s < %s > %s; result=$?; stty -g > %s; exit "$result"} $env(RESIZE_BEFORE) $binary $args $env(RESIZE_INPUT) $env(RESIZE_OUTPUT) $env(RESIZE_AFTER)]
        spawn -noecho /bin/sh -c $command
        expect_before timeout {
            catch {exec /bin/kill -KILL -- -[exp_pid]}
            catch {expect eof}
            exit 97
        }
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        expect -re {\x1b\[12;1H}
        after 100
        switch $surface {
            editor { send -- "X" }
            pager { send -- "\023alpha" }
            inspector { send -- "\033\[Be\02543" }
        }
        after 100
        # A resize must use its known cursor anchor, not consume typeahead while
        # waiting for another cursor-position report.
        send -- "\030^"
        expect -re {\x1b\[13;1H}
        # Shrink retains the bottom row and moves the top down by one: the
        # resulting view occupies rows 2..13, exactly twelve terminal rows.
        send -- "\030-"
        expect -exact "\033\[2;1H"
        expect -re {\x1b\[13;1H}
        # Burst input also verifies that resize does not swallow later keys.
        send -- "[string repeat "\030^\030-" 3]\033\[1;3A\033\[1;3B"
        switch $surface {
            editor { send -- "\030\023\030\003" }
            pager { send -- "\030\003" }
            picker - inspector { send -- "\r" }
            review { send -- "a" }
        }
        expect eof
        set result [wait]
        exit [lindex $result 3]
    "#;
    let process = Command::new("/usr/bin/expect")
        .args(["-c", script])
        .env("RESIZE_BINARY", binary)
        .env("RESIZE_SURFACE", surface)
        .env("RESIZE_TEXT", &text)
        .env("RESIZE_INPUT", &input)
        .env("RESIZE_OUTPUT", &output)
        .env("RESIZE_BEFORE", &before)
        .env("RESIZE_AFTER", &after)
        .output()
        .expect("run expect");
    assert!(
        process.status.success(),
        "{surface}: status {:?}\nstdout={}\nstderr={}",
        process.status.code(),
        String::from_utf8_lossy(&process.stdout),
        String::from_utf8_lossy(&process.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&process.stdout)
            .matches("\x1b[6n")
            .count(),
        1,
        "resizing must not query the cursor and consume queued input"
    );
    let result = fs::read_to_string(output).unwrap();
    assert_eq!(result.lines().count(), 1, "stdout remains one JSON record");
    assert!(!result.contains('\x1b'), "UI escapes belong to the TTY");
    assert_eq!(fs::read_to_string(input).unwrap(), contents);
    assert_eq!(
        fs::read_to_string(text).unwrap(),
        if surface == "editor" {
            "Xalpha\nbeta\n"
        } else {
            "alpha\nbeta\n"
        }
    );
    assert_eq!(
        terminal_mode(&fs::read_to_string(before).unwrap()),
        terminal_mode(&fs::read_to_string(after).unwrap())
    );
    serde_json::from_str(&result).unwrap()
}

fn terminal_mode(mode: &str) -> String {
    #[cfg(target_os = "macos")]
    return mode
        .trim()
        .split(':')
        .map(|field| {
            if let Some(flags) = field.strip_prefix("lflag=") {
                format!(
                    "lflag={:x}",
                    u64::from_str_radix(flags, 16).unwrap() & !(libc::PENDIN as u64)
                )
            } else {
                field.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(":");
    #[cfg(not(target_os = "macos"))]
    mode.trim().to_owned()
}

#[test]
fn editor_resize_preserves_text_and_save_quit_chords() {
    let result = exercise_resize(env!("CARGO_BIN_EXE_inmacs"), "editor");
    assert_eq!(result["edit_count"], 1);
    assert_eq!(result["outcome"], "saved");
}

#[test]
fn pager_resizes_while_incremental_search_is_active() {
    let result = exercise_resize(env!("CARGO_BIN_EXE_inpage"), "pager");
    assert_eq!(result["outcome"], "closed");
    assert_eq!(result["changed"], false);
}

#[test]
fn picker_resize_preserves_the_filtered_selection() {
    let result = exercise_resize(env!("CARGO_BIN_EXE_inpick"), "picker");
    assert_eq!(result["selection"]["id"], "beta");
}

#[test]
fn inspector_resize_preserves_an_in_progress_leaf_edit() {
    let result = exercise_resize(env!("CARGO_BIN_EXE_ininspect"), "inspector");
    assert_eq!(result["outcome"], "proposed");
    assert_eq!(result["proposal"]["new_value"], 43);
}

#[test]
fn review_resize_preserves_the_hunk_decision() {
    let result = exercise_resize(env!("CARGO_BIN_EXE_indiff"), "review");
    assert_eq!(result["outcome"], "accepted");
    assert_eq!(result["accepted_hunks"], serde_json::json!([0]));
}
