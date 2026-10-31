use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|_| {}));
    let mut arguments = std::env::args_os().skip(1);
    let probe = match arguments.next() {
        None => false,
        Some(argument) if argument == "--probe" && arguments.next().is_none() => true,
        _ => return failure(),
    };
    match std::panic::catch_unwind(|| vorra::memory::embedding::worker::run(probe)) {
        Ok(Ok(())) => ExitCode::SUCCESS,
        _ => failure(),
    }
}

fn failure() -> ExitCode {
    let _ = io::stderr().lock().write_all(b"embedding worker failed\n");
    ExitCode::FAILURE
}
