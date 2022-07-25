// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright Authors of bpfd

use aya::include_bytes_aligned;
use bpfd::server::{config_from_file, programs_from_directory, serve};
use log::warn;
use nix::{
    libc::RLIM_INFINITY,
    sys::resource::{setrlimit, Resource},
};
use simplelog::{ColorChoice, ConfigBuilder, LevelFilter, TermLogger, TerminalMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    TermLogger::init(
        LevelFilter::Debug,
        ConfigBuilder::new()
            .set_target_level(LevelFilter::Error)
            .set_location_level(LevelFilter::Error)
            .add_filter_ignore("h2".to_string())
            .add_filter_ignore("rustls".to_string())
            .add_filter_ignore("hyper".to_string())
            .add_filter_ignore("aya".to_string())
            .build(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )?;
    let dispatcher_bytes =
        include_bytes_aligned!("../../target/bpfel-unknown-none/release/xdp_dispatcher.bpf.o");
    setrlimit(Resource::RLIMIT_MEMLOCK, RLIM_INFINITY, RLIM_INFINITY).unwrap();

    let config = config_from_file("/etc/bpfd.toml");

    let static_programs = match programs_from_directory("/etc/bpfd/programs.d") {
        Ok(static_programs) => static_programs,
        // Bpfd should always start even if parsing static programs fails
        Err(e) => {
            warn!("Failed to parse program static files: {}", e);
            vec![]
        }
    };

    serve(config, dispatcher_bytes, static_programs).await?;
    Ok(())
}
