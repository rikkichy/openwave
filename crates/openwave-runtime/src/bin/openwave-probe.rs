fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(openwave_runtime::probe::cli() as u8)
}
