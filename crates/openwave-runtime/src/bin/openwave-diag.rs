fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(openwave_runtime::diag::cli() as u8)
}
