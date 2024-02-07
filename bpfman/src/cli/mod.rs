// SPDX-License-Identifier: Apache-2.0
// Copyright Authors of bpfman

pub(crate) mod args;
mod get;
mod image;
mod list;
mod load;
mod system;
mod table;
mod unload;

use args::Commands;
use bpfman_api::{config::Config, util::directories::RTPATH_BPFMAN_SOCKET};
use get::execute_get;
use list::execute_list;
use log::warn;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use unload::execute_unload;

impl Commands {
    pub(crate) async fn execute(&self, config: Config) -> Result<(), anyhow::Error> {
        match self {
            Commands::Load(l) => l.execute().await,
            Commands::Unload(args) => execute_unload(args).await,
            Commands::List(args) => execute_list(args).await,
            Commands::Get(args) => execute_get(args).await,
            Commands::Image(i) => i.execute().await,
            Commands::System(s) => s.execute(&config).await,
        }
    }
}

fn select_channel() -> Option<Channel> {
    let path = RTPATH_BPFMAN_SOCKET.to_string();

    let address = Endpoint::try_from(format!("unix:/{path}"));
    if let Err(e) = address {
        warn!("Failed to parse unix endpoint: {e:?}");
        return None;
    };
    let address = address.unwrap();
    let channel = address
        .connect_with_connector_lazy(service_fn(move |_: Uri| UnixStream::connect(path.clone())));
    Some(channel)
}
