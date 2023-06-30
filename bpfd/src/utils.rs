// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright Authors of bpfd

use std::{os::unix::fs::PermissionsExt, path::Path, str};

use bpfd_api::util::USRGRP_BPFD;
use log::{info, warn};
use nix::net::if_::if_nametoindex;
use tokio::{fs, io::AsyncReadExt};
use users::get_group_by_name;

use crate::errors::BpfdError;

// Like tokio::fs::read, but with O_NOCTTY set
pub(crate) async fn read<P: AsRef<Path>>(path: P) -> Result<Vec<u8>, BpfdError> {
    let mut data = vec![];
    tokio::fs::OpenOptions::new()
        .custom_flags(nix::libc::O_NOCTTY)
        .read(true)
        .open(path)
        .await
        .map_err(|e| BpfdError::Error(format!("can't open file: {e}")))?
        .read_to_end(&mut data)
        .await
        .map_err(|e| BpfdError::Error(format!("can't read file: {e}")))?;
    Ok(data)
}

// Like tokio::fs::read_to_string, but with O_NOCTTY set
pub(crate) async fn read_to_string<P: AsRef<Path>>(path: P) -> Result<String, BpfdError> {
    let mut buffer = String::new();
    tokio::fs::OpenOptions::new()
        .custom_flags(nix::libc::O_NOCTTY)
        .read(true)
        .open(path)
        .await
        .map_err(|e| BpfdError::Error(format!("can't open file: {e}")))?
        .read_to_string(&mut buffer)
        .await
        .map_err(|e| BpfdError::Error(format!("can't read file: {e}")))?;
    Ok(buffer)
}

pub(crate) fn get_ifindex(iface: &str) -> Result<u32, BpfdError> {
    match if_nametoindex(iface) {
        Ok(index) => {
            info!("Map {} to {}", iface, index);
            Ok(index)
        }
        Err(_) => {
            info!("Unable to validate interface {}", iface);
            Err(BpfdError::InvalidInterface)
        }
    }
}

pub(crate) async fn set_file_permissions(path: &str, mode: u32) {
    // Determine if User Group exists, if not, do nothing
    if get_group_by_name(USRGRP_BPFD).is_some() {
        // Set the permissions on the file based on input
        if (tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await).is_err()
        {
            warn!("Unable to set permissions on file {}. Continuing", path);
        }
    }
}

pub(crate) async fn set_dir_permissions(directory: &str, mode: u32) {
    // Determine if User Group exists, if not, do nothing
    if get_group_by_name(USRGRP_BPFD).is_some() {
        // Iterate through the files in the provided directory
        let mut entries = fs::read_dir(directory).await.unwrap();
        while let Some(file) = entries.next_entry().await.unwrap() {
            // Set the permissions on the file based on input
            set_file_permissions(&file.path().into_os_string().into_string().unwrap(), mode).await;
        }
    }
}

pub(crate) fn bytes_to_string<T>(raw_bytes: &[T]) -> String
where 
    T: num::Num + num::Zero + num::NumCast + PartialOrd + Copy,
 { 
    let length = raw_bytes
    .iter()
    .rposition(|ch| *ch != num::zero())
    .map(|pos| pos + 1)
    .unwrap_or(0);


    // The name field is defined as [std::os::raw::c_char; 16]. c_char may be signed or
    // unsigned depending on the platform; that's why we're using from_raw_parts here
    let raw_slice: &[u8] = unsafe { std::slice::from_raw_parts(raw_bytes.as_ptr() as *const _, length) };

    String::from_utf8_lossy(raw_slice).to_string()
}
