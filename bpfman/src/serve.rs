// SPDX-License-Identifier: Apache-2.0
// Copyright Authors of bpfman

use std::{fs::remove_file, path::Path};

use bpfman_api::{
    config::Config, util::directories::RTPATH_BPFMAN_SOCKET, v1::bpfman_server::BpfmanServer,
};
use log::{debug, info};
use tokio::{
    join,
    net::UnixListener,
    select,
    signal::unix::{signal, SignalKind},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use crate::{
    bpf::BpfManager,
    oci_utils::ImageManager,
    rpc::BpfmanLoader,
    storage::StorageManager,
    utils::{set_file_permissions, SOCK_MODE},
    ROOT_DB,
};

pub async fn serve(config: &Config, csi_support: bool) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel(32);

    let loader = BpfmanLoader::new(tx.clone());
    let path = RTPATH_BPFMAN_SOCKET.to_string();
    let service = BpfmanServer::new(loader);

    let mut listeners: Vec<_> = Vec::new();

    match serve_unix(path.clone(), service.clone()).await {
        Ok(handle) => listeners.push(handle),
        Err(e) => eprintln!("Error = {e:?}"),
    }

    let allow_unsigned = config.signing.as_ref().map_or(true, |s| s.allow_unsigned);
    let (itx, irx) = mpsc::channel(32);

    let mut image_manager = ImageManager::new(ROOT_DB.clone(), allow_unsigned, irx).await?;
    let image_manager_handle = tokio::spawn(async move {
        image_manager.run().await;
    });

    let mut bpf_manager = BpfManager::new(config.clone(), rx, itx);
    bpf_manager.rebuild_state().await?;

    // TODO(astoycos) see issue #881
    //let static_programs = get_static_programs(static_program_path).await?;

    // Load any static programs first
    // if !static_programs.is_empty() {
    //     for prog in static_programs {
    //         let ret_prog = bpf_manager.add_program(prog).await?;
    //         // Get the Kernel Info.
    //         let kernel_info = ret_prog
    //             .kernel_info()
    //             .expect("kernel info should be set for all loaded programs");
    //         info!("Loaded static program with program id {}", kernel_info.id)
    //     }
    // };

    if csi_support {
        let storage_manager = StorageManager::new(tx);
        let storage_manager_handle = tokio::spawn(async move { storage_manager.run().await });
        let (_, res_image, res_storage, _) = join!(
            join_listeners(listeners),
            image_manager_handle,
            storage_manager_handle,
            bpf_manager.process_commands()
        );
        if let Some(e) = res_storage.err() {
            return Err(e.into());
        }
        if let Some(e) = res_image.err() {
            return Err(e.into());
        }
    } else {
        let (_, res_image, _) = join!(
            join_listeners(listeners),
            image_manager_handle,
            bpf_manager.process_commands()
        );
        if let Some(e) = res_image.err() {
            return Err(e.into());
        }
    }

    Ok(())
}

pub(crate) async fn shutdown_handler() {
    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    select! {
        _ = sigint.recv() => {debug!("Received SIGINT")},
        _ = sigterm.recv() => {debug!("Received SIGTERM")},
    }
}

async fn join_listeners(listeners: Vec<JoinHandle<()>>) {
    for listener in listeners {
        match listener.await {
            Ok(()) => {}
            Err(e) => eprintln!("Error = {e:?}"),
        }
    }
}

async fn serve_unix(
    path: String,
    service: BpfmanServer<BpfmanLoader>,
) -> anyhow::Result<JoinHandle<()>> {
    // Listen on Unix socket
    if Path::new(&path).exists() {
        // Attempt to remove the socket, since bind fails if it exists
        remove_file(&path)?;
    }

    let uds = UnixListener::bind(&path)?;
    let uds_stream = UnixListenerStream::new(uds);
    // Always set the file permissions of our listening socket.
    set_file_permissions(&path.clone(), SOCK_MODE).await;

    let serve = Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(uds_stream, shutdown_handler());

    Ok(tokio::spawn(async move {
        info!("Listening on {path}");
        if let Err(e) = serve.await {
            eprintln!("Error = {e:?}");
        }
        info!("Shutdown Unix Handler {}", path);
    }))
}
