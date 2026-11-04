//! `gstore-bench` — see `bench/README.md` and `gstore-bench --help`.

fn main() -> std::process::ExitCode {
    gstore_bench::runner::cli::main()
}
