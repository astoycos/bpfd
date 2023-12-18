// SPDX-License-Identifier: Apache-2.0
// Copyright Authors of bpfman

use std::{
    fs::remove_file,
    os::unix::prelude::{FromRawFd, IntoRawFd},
    path::Path,
};

use anyhow::anyhow;
use bpfman_api::{
    config::{self, Config},
    util::directories::STDIR_DB,
    v1::bpfman_server::BpfmanServer,
};
use libsystemd::activation::IsType;
use log::{debug, info};
use sled::Config as DbConfig;
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
    static_program::get_static_programs,
    storage::StorageManager,
    utils::{set_file_permissions, SOCK_MODE},
};

pub async fn serve(
    config: &Config,
    static_program_path: &str,
    csi_support: bool,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel(32);

    let loader = BpfmanLoader::new(tx.clone());
    let service = BpfmanServer::new(loader);

    let mut listeners: Vec<_> = Vec::new();

    let mut use_activity_timer = false;

    for endpoint in &config.grpc.endpoints {
        match endpoint {
            config::Endpoint::Unix { path, enabled } => {
                if !enabled {
                    info!("Skipping disabled endpoint on {path}");
                    continue;
                }

                match serve_unix(path.clone(), service.clone(), &mut use_activity_timer).await {
                    Ok(handle) => listeners.push(handle),
                    Err(e) => eprintln!("Error = {e:?}"),
                }
            }
        }
    }

    let allow_unsigned = config.signing.as_ref().map_or(true, |s| s.allow_unsigned);
    let (itx, irx) = mpsc::channel(32);

    let database = DbConfig::default()
        .path(STDIR_DB)
        .open()
        .expect("Unable to open database");

    let mut image_manager = ImageManager::new(database.clone(), allow_unsigned, irx).await?;
    let image_manager_handle = tokio::spawn(async move {
        image_manager.run(use_activity_timer).await;
    });

    let mut bpf_manager = BpfManager::new(config.clone(), rx, itx, database);
    bpf_manager.rebuild_state().await?;

    let static_programs = get_static_programs(static_program_path).await?;

    // Load any static programs first
    if !static_programs.is_empty() {
        for prog in static_programs {
            let ret_prog = bpf_manager.add_program(prog).await?;
            // Get the Kernel Info.
            let kernel_info = ret_prog
                .kernel_info()
                .expect("kernel info should be set for all loaded programs");
            info!("Loaded static program with program id {}", kernel_info.id)
        }
    };

    if csi_support {
        let storage_manager = StorageManager::new(tx);
        let storage_manager_handle =
            tokio::spawn(async move { storage_manager.run(use_activity_timer).await });
        let (_, res_image, res_storage, _) = join!(
            join_listeners(listeners),
            image_manager_handle,
            storage_manager_handle,
            bpf_manager.process_commands(use_activity_timer)
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
            bpf_manager.process_commands(use_activity_timer)
        );
        if let Some(e) = res_image.err() {
            return Err(e.into());
        }
    }

    Ok(())
}

pub(crate) async fn shutdown_handler(use_activity_timer: bool) {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    let mut sigterm = signal(SignalKind::terminate()).unwrap();

    if use_activity_timer {
        let mut timer = Box::pin(tokio::time::sleep(TIMEOUT));

        select! {
            _ = sigint.recv() => {debug!("Received SIGINT")},
            _ = sigterm.recv() => {debug!("Received SIGTERM")},
            _ = &mut timer => {debug!("Inactivity timer expired")},
        }
    } else {
        select! {
            _ = sigint.recv() => {debug!("Received SIGINT")},
            _ = sigterm.recv() => {debug!("Received SIGTERM")},
        }
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
    use_activity_timer: &mut bool,
) -> anyhow::Result<JoinHandle<()>> {
    let uds_stream = if let Ok(stream) = systemd_unix_stream(path.clone()) {
        *use_activity_timer = true;
        stream
    } else {
        std_unix_stream(path.clone()).await?
    };

    let serve = Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(uds_stream, shutdown_handler(*use_activity_timer));

    Ok(tokio::spawn(async move {
        info!("Listening on {path}");
        if let Err(e) = serve.await {
            eprintln!("Error = {e:?}");
        }
        info!("Shutdown Unix Handler {}", path);
    }))
}

fn systemd_unix_stream(_path: String) -> anyhow::Result<UnixListenerStream> {
    let listen_fds = libsystemd::activation::receive_descriptors(true)?;
    if listen_fds.len() == 1 {
        if let Some(fd) = listen_fds.first() {
            if !fd.is_unix() {
                return Err(anyhow!("Wrong Socket"));
            }
            let std_listener =
                unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd.clone().into_raw_fd()) };
            std_listener.set_nonblocking(true)?;
            let tokio_listener = UnixListener::from_std(std_listener)?;
            info!("Using a Unix socket from systemd");
            return Ok(UnixListenerStream::new(tokio_listener));
        }
    }

    Err(anyhow!("Unable to retrieve fd from systemd"))
}

async fn std_unix_stream(path: String) -> anyhow::Result<UnixListenerStream> {
    // Listen on Unix socket
    if Path::new(&path).exists() {
        // Attempt to remove the socket, since bind fails if it exists
        remove_file(&path)?;
    }

    let uds = UnixListener::bind(&path)?;
    let stream = UnixListenerStream::new(uds);
    // Always set the file permissions of our listening socket.
    set_file_permissions(&path.clone(), SOCK_MODE).await;

    info!("Using default Unix socket");
    Ok(stream)
}
