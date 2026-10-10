//! The agent smoke suite: fixed notebook tasks run through a real agent, judged
//! from the MCP traffic and the notebooks the agent leaves (docs/smoke.md).
//!
//!     endeavor-smoke run [--only N1,N4] [--out DIR] [--julia PATH] [--depot DEPOT] [--retries N]
//!     endeavor-smoke mcp ARGS...    the recording proxy, started by the plugin as ENDEAVOR_BIN

mod checks;
mod inject;
mod log;
mod mcp;
mod proxy;
mod run;
mod ssh;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("mcp") => proxy::main(&args[1..]),
        Some("run") => run::main(&args[1..]),
        _ => {
            eprintln!("usage: endeavor-smoke run [--only IDS] [--out DIR] [--julia PATH] [--depot DEPOT] [--retries N]\n       endeavor-smoke mcp ARGS...   (the recording proxy; the runner sets it up)");
            2
        }
    };
    std::process::exit(code);
}
