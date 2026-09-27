#![cfg(unix)]
use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
#[test]
fn slow_application_keeps_native_editing_live_and_restores_terminal() {
    let root = std::env::temp_dir().join(format!(
        "inui-contract-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let script = root.join("bridge.py");
    fs::write(&script,r#"
import subprocess,os,json,time,threading
p=subprocess.Popen([os.environ['UI_BINARY'],'--height','16','--profile',os.environ['UI_TMP']+'/profile.json'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
def send(x):p.stdin.write(json.dumps(dict(schema_version=1,view='test',**x))+'\n');p.stdin.flush()
send(dict(type='init',revision=0,root={'kind':'panel','children':[{'kind':'list','key':'events','props':{'source':'events','field':'text'}},{'kind':'input','key':'draft','props':{'action':'send','clear_on_ack':True,'title':'Composer','border':True,'size':3}}]},collections=[{'id':'events','revision':0,'total':10000,'rows':[{'key':str(i),'fields':{'text':'event '+str(i)}} for i in range(10000)]}]))
requests=[]
for line in p.stdout:
 r=json.loads(line);requests.append(r)
 assert r['intent']=='action',r
 # Native keyboard and terminal paint must run during this intentional stall.
 time.sleep(2)
 send(dict(type='ack',request_id=r['request_id'],ok=True,message='ACKNOWLEDGED'))
open(os.environ['UI_TMP']+'/requests.json','w').write(json.dumps(requests))
p.wait()
"#).unwrap();
    let result=Command::new("/usr/bin/expect").args(["-c",r#"
set timeout 15
log_user 1
spawn -noecho /bin/sh -c {stty rows 30 columns 100; stty -g > "$UI_TMP/before"; python3 "$UI_TMP/bridge.py"; stty -g > "$UI_TMP/after"}
expect_before timeout {catch {exec /bin/kill -KILL -- -[exp_pid]};catch {expect eof};exit 97}
expect -exact "\033\[6n"
send -- "\033\[1;1R"
expect -exact "event 0"
send -- "\033\[B\033\[B\tfirst\r"
after 100
send -- "\033\[200~ newer\033\[201~"
set timeout 1
expect -exact "newer"
set timeout 10
expect -exact "ACKNOWLEDGED"
send -- "\033"
expect eof
exit [lindex [wait] 3]
"#]).env("UI_BINARY",env!("CARGO_BIN_EXE_inui")).env("UI_TMP",&root).output().unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        mode(fs::read_to_string(root.join("before")).unwrap()),
        mode(fs::read_to_string(root.join("after")).unwrap())
    );
    let requests: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(root.join("requests.json")).unwrap()).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["value"], "first");
    let profile: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("profile.json")).unwrap()).unwrap();
    assert_eq!(profile["counters"]["cached_rows"], 10000);
    assert!(
        profile["timings"]["local_input_to_terminal_write"]["max_us"]
            .as_u64()
            .unwrap()
            < 100000
    );
    fs::remove_dir_all(root).unwrap();
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
                    u64::from_str_radix(flags, 16).unwrap() & !libc::PENDIN
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
