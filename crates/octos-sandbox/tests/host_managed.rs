//! Real subprocess escape probes; no sandbox is installed in Cargo's runner.
#![allow(unsafe_code)]

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--probe") {
        probe(&args[2..]);
        return;
    }
    if args.get(1).map(String::as_str) == Some("--escaped") {
        std::process::exit(77);
    }
    run();
    if let Some(binary) = std::env::var_os("OCTOS_HOST_MANAGED_BINARY") {
        smoke_real_octos(std::path::Path::new(&binary));
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() {
    assert!(octos_sandbox::confine_host_managed().is_err());
    assert!(octos_sandbox::host_managed_command(&std::env::current_exe().unwrap()).is_err());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    let fixture = tempfile::tempdir().unwrap();
    let private = fixture.path().join("private.txt");
    std::fs::write(&private, "HOST SECRET MUST NOT BE READ").unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unix_path = fixture.path().join("host.sock");
    let _unix = std::os::unix::net::UnixListener::bind(&unix_path).unwrap();
    let leaked = std::fs::File::open(&private).unwrap();
    // Simulate a host dependency leaving a sensitive inheritable descriptor.
    assert_eq!(unsafe { libc::fcntl(leaked.as_raw_fd(), libc::F_SETFD, 0) }, 0);
    // Exercise literal escaping as well as the actual kernel boundary.
    let executable = fixture.path().join("octos worker\"with\\quotes");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    for mode in ["parent", "worker", "standalone"] {
        let mut command = if mode == "standalone" {
            let mut command = std::process::Command::new(&executable);
            command.env_clear().current_dir("/");
            command
        } else {
            octos_sandbox::host_managed_command(&executable).unwrap()
        };
        let mut child = command.args(["--probe", mode, private.to_str().unwrap(), unix_path.to_str().unwrap(),
                &tcp.local_addr().unwrap().to_string(), &leaked.as_raw_fd().to_string(),
                &std::process::id().to_string(), executable.to_str().unwrap()])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"host IPC input\n").unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success(), "{mode} probe failed: status={} stdout={} stderr={}",
            result.status, String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        assert_eq!(result.stdout, b"confined IPC output\n", "{mode} probe lost its IPC output");
    }
    // The probe must leave its host's files and listener intact.
    let mut contents = String::new();
    std::fs::File::open(&private).unwrap().read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "HOST SECRET MUST NOT BE READ");
    assert!(!fixture.path().join("escape.txt").exists());
    println!("host-managed parent, worker, and standalone escape probes passed");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn probe(args: &[String]) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let private = std::path::Path::new(&args[1]);
    let inherited_fd: i32 = args[4].parse().unwrap();
    let parent: i32 = args[5].parse().unwrap();
    if args[0] != "parent" { octos_sandbox::confine_host_managed().unwrap(); }
    assert!(std::fs::read(private).is_err(), "private file read escaped");
    assert!(std::fs::metadata(private).is_err(), "private file metadata escaped");
    assert!(std::fs::symlink_metadata(private).is_err(), "private symlink metadata escaped");
    assert!(std::fs::read_dir(private.parent().unwrap()).is_err(), "private directory read escaped");
    assert!(std::fs::write(private, "OVERWRITTEN").is_err(), "private write escaped");
    assert!(std::fs::write(private.with_file_name("escape.txt"), "EXFILTRATED").is_err(), "private file creation escaped");
    let mut byte = 0u8;
    assert_eq!(unsafe { libc::read(inherited_fd, (&mut byte as *mut u8).cast(), 1) }, -1, "sensitive descriptor was inherited");
    assert!(std::net::TcpStream::connect(&args[3]).is_err(), "TCP escaped");
    assert!(std::net::UdpSocket::bind("127.0.0.1:0").is_err(), "UDP escaped");
    assert!(std::os::unix::net::UnixStream::connect(&args[2]).is_err(), "Unix socket escaped");
    #[cfg(target_os = "linux")]
    assert!(std::os::unix::net::UnixDatagram::pair().is_err(), "addressable datagram pair escaped");
    assert!(std::process::Command::new("/bin/sh").args(["-c", "exit 0"]).status().is_err(), "subprocess escaped");
    let executable = if cfg!(target_os = "linux") && args[0] != "standalone" { "/app/octos" } else { &args[6] };
    assert!(std::process::Command::new(executable).arg("--escaped").status().is_err(),
        "same-executable subprocess escaped");
    check_parent_memory(parent);

    // Thread creation and Tokio stdio must still work after confinement.
    std::thread::spawn(|| assert_eq!(2 + 2, 4)).join().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut input = String::new();
        tokio::io::stdin().read_to_string(&mut input).await.unwrap();
        assert_eq!(input, "host IPC input\n");
        let mut stdout = tokio::io::stdout();
        stdout.write_all(b"confined IPC output\n").await.unwrap();
        stdout.flush().await.unwrap();
    });
    if args[0] == "standalone" || (args[0] == "worker" && cfg!(target_os = "linux")) {
        // No self-reexec loophole remains after the child's second restriction.
        use std::os::unix::process::CommandExt;
        let executable = if cfg!(target_os = "linux") && args[0] == "worker" { "/app/octos" } else { &args[6] };
        let error = std::process::Command::new(executable).arg("--escaped").exec();
        assert!(matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::ENOSYS)), "unexpected exec error: {error}");
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn check_parent_memory(parent: i32) {
    let mut task = 0;
    assert_ne!(unsafe { libc::task_for_pid(libc::mach_task_self(), parent, &mut task) }, 0,
        "parent task port escaped");
    unsafe extern "C" {
        static bootstrap_port: u32;
        fn bootstrap_look_up(port: u32, name: *const libc::c_char, result: *mut u32) -> i32;
    }
    let mut service = 0;
    assert_ne!(unsafe { bootstrap_look_up(bootstrap_port, c"com.apple.securityd".as_ptr(), &mut service) }, 0,
        "keychain Mach service escaped");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn smoke_real_octos(binary: &std::path::Path) {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use std::process::Stdio;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let mut command = octos_sandbox::host_managed_command(binary).unwrap();
        command.args(["acp", "--host-managed"]).stdin(Stdio::piped())
            .stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = tokio::process::Command::from(command).kill_on_drop(true).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut stderr = child.stderr.take().unwrap();
        let diagnostic = tokio::spawn(async move {
            let mut output = Vec::new();
            (&mut stderr).take(8192).read_to_end(&mut output).await.unwrap();
            String::from_utf8_lossy(&output).into_owned()
        });
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":1,"clientCapabilities":{"_meta":{"octos.hostManaged":{
                "version":1,"model":{"model_id":"smoke","provider_name":"host",
                    "context_window":32000,"max_output_tokens":1024},"system_prompt":"Synthetic test only."
            }}}
        }});
        stdin.write_all(format!("{initialize}\n").as_bytes()).await.unwrap();
        let line = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line()).await.unwrap().unwrap();
        let Some(line) = line else { panic!("Octos startup failed: {}", diagnostic.await.unwrap()); };
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["result"]["agentCapabilities"]["_meta"]["octos.hostManaged"]["confined"], true,
            "Octos did not confirm confinement: {response}");
        let session = json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/","mcpServers":[]}});
        stdin.write_all(format!("{session}\n").as_bytes()).await.unwrap();
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line()).await.unwrap().unwrap();
            let Some(line) = line else { panic!("Octos session failed: {}", diagnostic.await.unwrap()); };
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["method"] == "_octos/host/tools/list" {
                let response = json!({"jsonrpc":"2.0","id":frame["id"],"result":{"tools":[]}});
                stdin.write_all(format!("{response}\n").as_bytes()).await.unwrap();
            } else {
                assert_eq!(frame["id"], 2, "unexpected Octos frame: {frame}");
                assert!(frame["result"]["sessionId"].is_string(), "Octos session failed: {frame}");
                break;
            }
        }
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        diagnostic.await.unwrap();
    });
    println!("real confined Octos handshake and memory-only session passed");
}

#[cfg(target_os = "linux")]
fn check_parent_memory(parent: i32) {
    assert!(std::fs::read(format!("/proc/{parent}/environ")).is_err(), "parent environment escaped");
    assert_eq!(unsafe { libc::ptrace(libc::PTRACE_ATTACH, parent, 0, 0) }, -1,
        "parent ptrace escaped");
}
