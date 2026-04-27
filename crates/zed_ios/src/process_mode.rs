use anyhow::{Context as _, Result, anyhow};
use remote::{RusshHelperFrame, RusshHelperRequest, RusshWindowSize};
use std::{
    ffi::{CStr, c_char},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

static SIGWINCH_PENDING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq, Eq)]
struct RusshHelperExecArgs {
    socket_path: PathBuf,
    interactive: bool,
    command: String,
}

pub(crate) fn maybe_run_process_mode(argc: i32, argv: *const *const c_char) -> i32 {
    match parse_ffi_argv(argc, argv).and_then(|args| maybe_run_process_mode_from_args(&args)) {
        Ok(Some(exit_code)) => exit_code,
        Ok(None) => -1,
        Err(error) => {
            eprintln!("[Zed iOS] process mode failed: {error:#}");
            1
        }
    }
}

fn maybe_run_process_mode_from_args(args: &[String]) -> Result<Option<i32>> {
    let Some(exec_args) = parse_process_mode_args(args)? else {
        return Ok(None);
    };

    Ok(Some(run_russh_helper_exec(&exec_args)?))
}

fn parse_process_mode_args(args: &[String]) -> Result<Option<RusshHelperExecArgs>> {
    if args.get(1).map(String::as_str) != Some("--zed-ios-russh-helper") {
        return Ok(None);
    }
    if args.get(2).map(String::as_str) != Some("exec") {
        anyhow::bail!("Unsupported iOS Russh helper subcommand");
    }

    let mut socket_path = None;
    let mut interactive = None;
    let mut command = None;
    let mut index = 3;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("Missing value for --socket"))?;
                socket_path = Some(PathBuf::from(value));
            }
            "--interactive" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("Missing value for --interactive"))?;
                interactive = Some(match value.as_str() {
                    "yes" => true,
                    "no" => false,
                    _ => anyhow::bail!("Invalid value for --interactive: {value}"),
                });
            }
            "--command" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("Missing value for --command"))?;
                command = Some(value.clone());
            }
            other => {
                anyhow::bail!("Unexpected iOS Russh helper argument: {other}");
            }
        }
        index += 1;
    }

    Ok(Some(RusshHelperExecArgs {
        socket_path: socket_path.ok_or_else(|| anyhow!("Missing --socket"))?,
        interactive: interactive.ok_or_else(|| anyhow!("Missing --interactive"))?,
        command: command.ok_or_else(|| anyhow!("Missing --command"))?,
    }))
}

fn run_russh_helper_exec(args: &RusshHelperExecArgs) -> Result<i32> {
    let mut socket = UnixStream::connect(&args.socket_path).with_context(|| {
        format!(
            "Failed to connect to Russh helper socket at {}",
            args.socket_path.display()
        )
    })?;

    let initial_window_size = current_stdin_window_size();
    let request = build_helper_request(args, initial_window_size);
    request.write_to(&mut socket)?;

    let writer = Arc::new(Mutex::new(
        socket
            .try_clone()
            .context("Failed to clone Russh helper socket")?,
    ));
    let should_stop = Arc::new(AtomicBool::new(false));

    spawn_stdin_proxy(writer.clone());
    if args.interactive && initial_window_size.is_some() {
        install_sigwinch_handler();
        spawn_resize_watcher(writer.clone(), should_stop.clone());
    }

    let exit_status = proxy_socket_output(&mut socket)?;
    should_stop.store(true, Ordering::SeqCst);
    Ok(exit_status)
}

fn build_helper_request(
    args: &RusshHelperExecArgs,
    initial_window_size: Option<RusshWindowSize>,
) -> RusshHelperRequest {
    RusshHelperRequest {
        command: args.command.clone(),
        interactive: args.interactive,
        initial_window_size,
    }
}

fn proxy_socket_output(socket: &mut UnixStream) -> Result<i32> {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut stdout = stdout.lock();
    let mut stderr = stderr.lock();
    let mut exit_status = None;

    loop {
        match RusshHelperFrame::read_from(socket) {
            Ok(RusshHelperFrame::Stdout(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Ok(RusshHelperFrame::Stderr(data)) => {
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            Ok(RusshHelperFrame::ExitStatus(status)) => {
                exit_status = Some(status);
            }
            Ok(RusshHelperFrame::Error(message)) => {
                writeln!(stderr, "{message}")?;
                return Ok(1);
            }
            Ok(frame) => {
                anyhow::bail!("Unexpected frame from Russh helper parent: {frame:?}");
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                break;
            }
            Err(error) => return Err(error).context("Failed to read Russh helper response"),
        }
    }

    Ok(exit_status.unwrap_or(1))
}

fn spawn_stdin_proxy(writer: Arc<Mutex<UnixStream>>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut buffer = [0; 8192];

        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => {
                    let _ = send_frame(&writer, RusshHelperFrame::Eof);
                    break;
                }
                Ok(bytes_read) => {
                    if send_frame(
                        &writer,
                        RusshHelperFrame::Stdin(buffer[..bytes_read].to_vec()),
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
}

fn spawn_resize_watcher(writer: Arc<Mutex<UnixStream>>, should_stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        while !should_stop.load(Ordering::SeqCst) {
            if SIGWINCH_PENDING.swap(false, Ordering::SeqCst) {
                if let Some(window_size) = current_stdin_window_size() {
                    if send_frame(&writer, RusshHelperFrame::Resize(window_size)).is_err() {
                        break;
                    }
                }
            }

            thread::sleep(Duration::from_millis(50));
        }
    });
}

fn send_frame(writer: &Arc<Mutex<UnixStream>>, frame: RusshHelperFrame) -> Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| anyhow!("Russh helper socket mutex was poisoned"))?;
    frame.write_to(&mut *writer)
}

fn current_stdin_window_size() -> Option<RusshWindowSize> {
    if !stdin_is_tty() {
        return None;
    }

    let mut raw_window_size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, raw_window_size.as_mut_ptr()) }
        != 0
    {
        return None;
    }

    let raw_window_size = unsafe { raw_window_size.assume_init() };
    if raw_window_size.ws_col == 0 || raw_window_size.ws_row == 0 {
        return None;
    }

    Some(RusshWindowSize {
        columns: raw_window_size.ws_col,
        rows: raw_window_size.ws_row,
        pixel_width: raw_window_size.ws_xpixel,
        pixel_height: raw_window_size.ws_ypixel,
    })
}

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

unsafe extern "C" fn handle_sigwinch(_: i32) {
    SIGWINCH_PENDING.store(true, Ordering::SeqCst);
}

fn install_sigwinch_handler() {
    SIGWINCH_PENDING.store(false, Ordering::SeqCst);
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            handle_sigwinch as *const () as libc::sighandler_t,
        );
    }
}

fn parse_ffi_argv(argc: i32, argv: *const *const c_char) -> Result<Vec<String>> {
    if argc < 0 {
        anyhow::bail!("Negative argc passed to zed_ios_maybe_run_process_mode");
    }
    if argc == 0 {
        return Ok(Vec::new());
    }
    if argv.is_null() {
        anyhow::bail!("Null argv passed to zed_ios_maybe_run_process_mode");
    }

    let mut args = Vec::with_capacity(argc as usize);
    for index in 0..argc as isize {
        let argument = unsafe {
            let argument = *argv.offset(index);
            if argument.is_null() {
                anyhow::bail!("Null argv element passed to zed_ios_maybe_run_process_mode");
            }
            CStr::from_ptr(argument).to_string_lossy().into_owned()
        };
        args.push(argument);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::{RusshHelperExecArgs, build_helper_request, parse_process_mode_args};
    use remote::RusshWindowSize;
    use std::path::PathBuf;

    #[test]
    fn parses_russh_helper_exec_argv() {
        let args = vec![
            "/Applications/Zed.app/Zed".into(),
            "--zed-ios-russh-helper".into(),
            "exec".into(),
            "--socket".into(),
            "/tmp/zed-russh/helper.sock".into(),
            "--interactive".into(),
            "yes".into(),
            "--command".into(),
            "cd \"$HOME/project\" && exec env bash -l".into(),
        ];

        let parsed = parse_process_mode_args(&args).unwrap().unwrap();
        assert_eq!(parsed.socket_path, PathBuf::from("/tmp/zed-russh/helper.sock"));
        assert!(parsed.interactive);
        assert_eq!(parsed.command, "cd \"$HOME/project\" && exec env bash -l");
    }

    #[test]
    fn non_helper_argv_is_not_handled() {
        let args = vec!["/Applications/Zed.app/Zed".into(), "--regular".into()];
        assert!(parse_process_mode_args(&args).unwrap().is_none());
    }

    #[test]
    fn helper_request_construction_preserves_interactive_flag_and_command() {
        let args = RusshHelperExecArgs {
            socket_path: PathBuf::from("/tmp/zed-russh/helper.sock"),
            interactive: false,
            command: "exec env printf hello".into(),
        };
        let request = build_helper_request(
            &args,
            Some(RusshWindowSize {
                columns: 100,
                rows: 40,
                pixel_width: 1200,
                pixel_height: 800,
            }),
        );

        assert!(!request.interactive);
        assert_eq!(request.command, "exec env printf hello");
        assert_eq!(
            request.initial_window_size,
            Some(RusshWindowSize {
                columns: 100,
                rows: 40,
                pixel_width: 1200,
                pixel_height: 800,
            })
        );
    }
}
