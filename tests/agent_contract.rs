#![cfg(unix)]
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn exercise(signal: bool) -> Vec<serde_json::Value> {
    let scratch = Scratch(std::env::temp_dir().join(format!(
            "inagent-contract-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir(&scratch.0).unwrap();
    let bridge = scratch.0.join("bridge.py");
    let output = scratch.0.join("intents.jsonl");
    let before = scratch.0.join("before");
    let after = scratch.0.join("after");
    let pid = scratch.0.join("pid");
    fs::write(&bridge,r#"
import subprocess,sys,json,os,threading,time
p=subprocess.Popen([os.environ['AGENT_BINARY'],'--height','16'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
open(os.environ['AGENT_PID'],'w').write(str(p.pid))
snapshot={'schema_version':1,'type':'snapshot','session':{'id':'session_test','title':'Gusgus','workspace':'/repo','profile':'jcode','lifecycle':'open','activity':'running','run_id':'run_original','pending':0},'has_earlier':True,'window':400,'entries':[{'id':'one','kind':'assistant','title':'Assistant','text':'backlog visible'}]}
lock=threading.Lock()
def send(frame):
 with lock:
  p.stdin.write(json.dumps(frame)+'\n');p.stdin.flush()
send(snapshot)
def update():
 time.sleep(.4)
 snapshot['entries'].append({'id':'two','kind':'assistant','title':'Assistant','text':'LIVE UPDATE'})
 send(snapshot)
threading.Thread(target=update,daemon=True).start()
for line in p.stdout:
 print(line,end='',flush=True)
 frame=json.loads(line)
 if frame['intent']=='dismiss':break
 send({'schema_version':1,'type':'ack','request_id':frame['request_id'],'ok':True,'message':'Message queued'})
p.stdin.close()
sys.exit(p.wait())
"#).unwrap();
    let script = r#"
        log_user 1
        set timeout 15
        set command [format {stty rows 30 columns 110; stty -g > %s; python3 %s > %s; status=$?; stty -g > %s; exit "$status"} $env(AGENT_BEFORE) $env(AGENT_BRIDGE) $env(AGENT_OUTPUT) $env(AGENT_AFTER)]
        spawn -noecho /bin/sh -c $command
        expect_before timeout {catch {exec /bin/kill -KILL -- -[exp_pid]};catch {expect eof};exit 97}
        expect -exact "\033\[6n"
        send -- "\033\[1;1R"
        expect -exact "backlog visible"
        expect -exact "LIVE UPDATE"
        if {$env(AGENT_SIGNAL)=="1"} {
            set file [open $env(AGENT_PID) r];set pid [read $file];close $file
            exec /bin/kill -TERM $pid
        } else {
            send -- "literal dollar \$ and quotes \""
            send -- "\rsecond line"
            # Resize uses the shared implementation and preserves the draft.
            send -- "\030^\030-"
            send -- "\003\003"
            expect -exact "queued"
            send -- "unsent"
            send -- "\030\003"
            after 150
            send -- "n"
            after 100
            send -- "\030\003"
            after 100
            send -- "y"
        }
        expect eof
        set result [wait]
        exit [lindex $result 3]
    "#;
    let result = Command::new("/usr/bin/expect")
        .args(["-c", script])
        .env("AGENT_BINARY", env!("CARGO_BIN_EXE_inagent"))
        .env("AGENT_BRIDGE", &bridge)
        .env("AGENT_OUTPUT", &output)
        .env("AGENT_BEFORE", &before)
        .env("AGENT_AFTER", &after)
        .env("AGENT_PID", &pid)
        .env("AGENT_SIGNAL", if signal { "1" } else { "0" })
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        mode(fs::read_to_string(before).unwrap()),
        mode(fs::read_to_string(after).unwrap()),
        "terminal mode restored"
    );
    let text = fs::read_to_string(output).unwrap();
    assert!(!text.contains('\x1b'), "stdout is protocol only");
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn mode(mode: String) -> String {
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
                field.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(":");
    #[cfg(not(target_os = "macos"))]
    mode
}
#[test]
fn live_view_composer_resize_and_detach_use_separate_channels() {
    let frames = exercise(false);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["intent"], "send_message");
    assert_eq!(
        frames[0]["body"],
        "literal dollar $ and quotes \"\nsecond line"
    );
    assert_eq!(frames[1]["intent"], "dismiss");
}
#[test]
fn signal_restores_terminal_without_a_stop_intent() {
    assert!(exercise(true).is_empty());
}
