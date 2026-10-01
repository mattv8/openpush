use openpush_protocol::write_contracts;
use std::{env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut check = false;
    let mut root = env::current_dir().expect("current directory");
    let args: Vec<_> = env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--check" => check = true,
            "--out-root" => {
                index += 1;
                root = PathBuf::from(args.get(index).expect("--out-root needs a directory"));
            }
            value => {
                eprintln!("unknown argument: {value}");
                return ExitCode::FAILURE;
            }
        };
        index += 1;
    }
    match write_contracts(&root, check) {
        Ok(_) if !check => ExitCode::SUCCESS,
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("generated contracts are out of date");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("contract generation failed: {error}");
            ExitCode::FAILURE
        }
    }
}
