// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright Authors of bpfd

use std::{collections::HashMap, convert::TryInto};

use anyhow::anyhow;
use aya::{
    programs::{
        links::FdLink, loaded_programs, trace_point::TracePointLink, uprobe::UProbeLink,
        TracePoint, UProbe,
    },
    BpfLoader,
};
use bpfd_api::{config::Config, util::directories::*, ProgramType};
use log::{debug, info};
use tokio::{fs, select, sync::mpsc};
use uuid::Uuid;

use crate::{
    command::{
        self, BpfMap, Command, Direction,
        Direction::{Egress, Ingress},
        LoadTCArgs, LoadTracepointArgs, LoadUprobeArgs, LoadXDPArgs, Program, ProgramData,
        ProgramInfo, PullBytecodeArgs, TcProgram, TcProgramInfo, TracepointProgram,
        TracepointProgramInfo, UnloadArgs, UprobeProgram, UprobeProgramInfo, XdpProgram,
        XdpProgramInfo,
    },
    errors::BpfdError,
    multiprog::{Dispatcher, DispatcherId, DispatcherInfo, TcDispatcher, XdpDispatcher},
    oci_utils::image_manager::get_bytecode_from_image_store,
    serve::shutdown_handler,
    utils::{get_ifindex, read, set_dir_permissions},
};

const SUPERUSER: &str = "bpfctl";
const MAPS_MODE: u32 = 0o0660;

pub(crate) struct BpfManager {
    config: Config,
    dispatchers: HashMap<DispatcherId, Dispatcher>,
    programs: HashMap<Uuid, Program>,
    maps: HashMap<String, BpfMap>,
    commands: mpsc::Receiver<Command>,
}

impl BpfManager {
    pub(crate) fn new(config: Config, commands: mpsc::Receiver<Command>) -> Self {
        Self {
            config,
            dispatchers: HashMap::new(),
            programs: HashMap::new(),
            maps: HashMap::new(),
            commands,
        }
    }

    pub(crate) async fn rebuild_state(&mut self) -> Result<(), anyhow::Error> {
        debug!("BpfManager::rebuild_state()");
        let mut programs_dir = fs::read_dir(RTDIR_PROGRAMS).await?;
        while let Some(entry) = programs_dir.next_entry().await? {
            let uuid = entry.file_name().to_string_lossy().parse().unwrap();
            let mut program = Program::load(uuid)
                .map_err(|e| BpfdError::Error(format!("cant read program state {e}")))?;
            // TODO: Should probably check for pinned prog on bpffs rather than assuming they are attached
            program.set_attached();
            debug!("rebuilding state for program {}", uuid);
            self.rebuild_map_entry(uuid, program.data().map_prog_uuid.clone());
            self.programs.insert(uuid, program);
        }
        self.rebuild_dispatcher_state(ProgramType::Xdp, None, RTDIR_XDP_DISPATCHER)
            .await?;
        self.rebuild_dispatcher_state(ProgramType::Tc, Some(Ingress), RTDIR_TC_INGRESS_DISPATCHER)
            .await?;
        self.rebuild_dispatcher_state(ProgramType::Tc, Some(Egress), RTDIR_TC_EGRESS_DISPATCHER)
            .await?;

        Ok(())
    }

    pub(crate) async fn rebuild_dispatcher_state(
        &mut self,
        program_type: ProgramType,
        direction: Option<Direction>,
        path: &str,
    ) -> Result<(), anyhow::Error> {
        let mut dispatcher_dir = fs::read_dir(path).await?;
        while let Some(entry) = dispatcher_dir.next_entry().await? {
            let name = entry.file_name();
            let parts: Vec<&str> = name.to_str().unwrap().split('_').collect();
            if parts.len() != 2 {
                continue;
            }
            let if_index: u32 = parts[0].parse().unwrap();
            let revision: u32 = parts[1].parse().unwrap();
            match program_type {
                ProgramType::Xdp => {
                    let dispatcher = XdpDispatcher::load(if_index, revision).unwrap();
                    self.dispatchers.insert(
                        DispatcherId::Xdp(DispatcherInfo(if_index, None)),
                        Dispatcher::Xdp(dispatcher),
                    );
                }
                ProgramType::Tc => {
                    if let Some(dir) = direction {
                        let dispatcher = TcDispatcher::load(if_index, dir, revision).unwrap();
                        self.dispatchers.insert(
                            DispatcherId::Tc(DispatcherInfo(if_index, direction)),
                            Dispatcher::Tc(dispatcher),
                        );
                    } else {
                        return Err(anyhow!("direction required for tc programs"));
                    }

                    self.rebuild_multiattach_dispatcher(
                        program_type,
                        if_index,
                        direction,
                        DispatcherId::Tc(DispatcherInfo(if_index, direction)),
                    )
                    .await?;
                }
                _ => return Err(anyhow!("invalid program type {:?}", program_type)),
            }
        }

        Ok(())
    }

    pub(crate) async fn add_program(
        &mut self,
        program: Program,
        id: Option<Uuid>,
    ) -> Result<Uuid, BpfdError> {
        debug!("BpfManager::add_program()");

        let uuid = match id {
            Some(id) => {
                debug!("Using provided program UUID: {}", id);
                if self.programs.contains_key(&id) {
                    return Err(BpfdError::PassedUUIDInUse(id));
                }
                id
            }
            None => {
                debug!("Generating new program UUID");
                Uuid::new_v4()
            }
        };

        match program {
            Program::Xdp(_) | Program::Tc(_) => self.add_multi_attach_program(program, uuid).await,
            Program::Tracepoint(_) | Program::Uprobe(_) => {
                self.add_single_attach_program(program, uuid).await
            }
        }
    }

    pub(crate) async fn add_multi_attach_program(
        &mut self,
        program: Program,
        id: Uuid,
    ) -> Result<Uuid, BpfdError> {
        debug!("BpfManager::add_multi_attach_program()");

        let map_prog_uuid = program.data().map_prog_uuid.clone();
        let map_pin_path = self.manage_map_pin_path(id, map_prog_uuid.clone()).await?;

        let program_bytes = if program
            .data()
            .path
            .clone()
            .contains(BYTECODE_IMAGE_CONTENT_STORE)
        {
            get_bytecode_from_image_store(program.data().path.clone()).await?
        } else {
            read(program.data().path.clone()).await?
        };

        let mut ext_loader = BpfLoader::new()
            .extension(&program.data().section_name)
            .map_pin_path(map_pin_path.clone())
            .load(&program_bytes)?;

        match ext_loader.program_mut(&program.data().section_name) {
            Some(_) => Ok(()),
            None => {
                let _ = self.cleanup_map_pin_path(id, map_prog_uuid.clone()).await;
                Err(BpfdError::SectionNameNotValid(
                    program.data().section_name.clone(),
                ))
            }
        }?;

        // Calculate the next_available_id
        let next_available_id = self
            .programs
            .iter()
            .filter(|(_, p)| {
                if p.kind() == program.kind() {
                    p.if_index() == program.if_index() && p.direction() == program.direction()
                } else {
                    false
                }
            })
            .collect::<HashMap<_, _>>()
            .len();
        if next_available_id >= 10 {
            return Err(BpfdError::TooManyPrograms);
        }

        debug!("next_available_id={next_available_id}");

        let program_type = program.kind();
        let if_index = program.if_index();
        let if_name = program.if_name().unwrap();
        let direction = program.direction();

        let did = program
            .dispatcher_id()
            .ok_or(BpfdError::DispatcherNotRequired)?;

        self.programs.insert(id, program);
        self.sort_programs(program_type, if_index, direction);
        let mut programs = self.collect_programs(program_type, if_index, direction);
        let old_dispatcher = self.dispatchers.remove(&did);
        let if_config = if let Some(ref i) = self.config.interfaces {
            i.get(&if_name)
        } else {
            None
        };
        let next_revision = if let Some(ref old) = old_dispatcher {
            old.next_revision()
        } else {
            1
        };
        let dispatcher = Dispatcher::new(if_config, &mut programs, next_revision, old_dispatcher)
            .await
            .or_else(|e| {
                let prog = self.programs.remove(&id).unwrap();
                prog.delete(id).map_err(|_| {
                    BpfdError::Error(
                        "new program cleanup failed, unable to delete program data".to_string(),
                    )
                })?;
                Err(e)
            })?;
        self.dispatchers.insert(did, dispatcher);

        // update programs with now populated kernel info
        // TODO this data flow should be optimized so that we don't have
        // to re-iterate through the programs.
        programs.iter().for_each(|(i, p)| {
            self.programs.insert(i.to_owned(), p.to_owned());
        });

        if let Some(p) = self.programs.get_mut(&id) {
            p.set_attached();
            p.save(id)
                .map_err(|e| BpfdError::Error(format!("unable to save program state: {e}")))?;
        };

        // Now that program is successfully loaded, update the maps hash table,
        // and allow access to all maps by bpfd group members.
        self.save_map(id, map_prog_uuid, map_pin_path.clone())
            .await?;

        Ok(id)
    }

    pub(crate) async fn add_single_attach_program(
        &mut self,
        mut p: Program,
        id: Uuid,
    ) -> Result<Uuid, BpfdError> {
        debug!("BpfManager::add_single_attach_program()");
        let map_prog_uuid = p.data().map_prog_uuid.clone();
        let map_pin_path = self.manage_map_pin_path(id, map_prog_uuid.clone()).await?;
        let program_bytes = if p.data().path.clone().contains(BYTECODE_IMAGE_CONTENT_STORE) {
            get_bytecode_from_image_store(p.data().path.clone()).await?
        } else {
            read(p.data().path.clone()).await?
        };

        let mut loader = BpfLoader::new();

        for (name, value) in &p.data().global_data {
            loader.set_global(name, value.as_slice(), true);
        }

        let mut loader = loader
            .map_pin_path(map_pin_path.clone())
            .load(&program_bytes)?;

        let raw_program =
            loader
                .program_mut(&p.data().section_name)
                .ok_or(BpfdError::SectionNameNotValid(
                    p.data().section_name.clone(),
                ))?;

        match p.clone() {
            Program::Tracepoint(program) => {
                let parts: Vec<&str> = program.info.tracepoint.split('/').collect();
                if parts.len() != 2 {
                    return Err(BpfdError::InvalidAttach(
                        program.info.tracepoint.to_string(),
                    ));
                }
                let category = parts[0].to_owned();
                let name = parts[1].to_owned();

                let tracepoint: &mut TracePoint = raw_program.try_into()?;

                tracepoint.load()?;
                p.set_kernel_info(tracepoint.program_info()?.try_into()?);
                p.save(id)
                    .map_err(|_| BpfdError::Error("unable to persist program data".to_string()))?;
                self.programs.insert(id, p);

                let link_id = tracepoint.attach(&category, &name).or_else(|e| {
                    let prog = self.programs.remove(&id).unwrap();
                    prog.delete(id).map_err(|_| {
                        BpfdError::Error(
                            "new program cleanup failed, unable to delete program data".to_string(),
                        )
                    })?;
                    Err(BpfdError::BpfProgramError(e))
                })?;

                let owned_link: TracePointLink = tracepoint.take_link(link_id)?;
                let fd_link: FdLink = owned_link
                    .try_into()
                    .expect("unable to get owned tracepoint attach link");
                fd_link
                    .pin(format!("{RTDIR_FS}/prog_{}_link", id))
                    .map_err(BpfdError::UnableToPinLink)?;

                tracepoint
                    .pin(format!("{RTDIR_FS}/prog_{id}"))
                    .or_else(|e| {
                        let prog = self.programs.remove(&id).unwrap();
                        prog.delete(id).map_err(|_| {
                            BpfdError::Error(
                                "new dispatcher cleanup failed, unable to delete program data"
                                    .to_string(),
                            )
                        })?;
                        Err(BpfdError::UnableToPinProgram(e))
                    })?;

                // Now that program is successfully loaded, update the maps hash table,
                // and allow access to all maps by bpfd group members.
                self.save_map(id, map_prog_uuid, map_pin_path.clone())
                    .await?;

                Ok(id)
            }
            Program::Uprobe(program) => {
                let uprobe: &mut UProbe = raw_program.try_into()?;

                uprobe.load()?;
                p.set_kernel_info(uprobe.program_info()?.try_into()?);
                p.save(id)
                    .map_err(|_| BpfdError::Error("unable to persist program data".to_string()))?;

                let link_id = uprobe
                    .attach(
                        program.info.fn_name.as_deref(),
                        program.info.offset.unwrap_or_default(),
                        program.info.target.clone(),
                        program.info.pid,
                    )
                    .or_else(|e| {
                        p.delete(id).map_err(|_| {
                            BpfdError::Error(
                                "new dispatcher cleanup failed, unable to delete program data"
                                    .to_string(),
                            )
                        })?;
                        Err(BpfdError::BpfProgramError(e))
                    })?;

                self.programs.insert(id, p);

                let owned_link: UProbeLink = uprobe.take_link(link_id)?;
                let fd_link: FdLink = owned_link
                    .try_into()
                    .expect("unable to get owned uprobe attach link");
                fd_link
                    .pin(format!("{RTDIR_FS}/prog_{}_link", id))
                    .map_err(BpfdError::UnableToPinLink)?;

                uprobe.pin(format!("{RTDIR_FS}/prog_{id}")).or_else(|e| {
                    let prog = self.programs.remove(&id).unwrap();
                    prog.delete(id).map_err(|_| {
                        BpfdError::Error(
                            "new program cleanup failed, unable to delete program data".to_string(),
                        )
                    })?;
                    Err(BpfdError::UnableToPinProgram(e))
                })?;

                // Now that program is successfully loaded, update the maps hash table,
                // and allow access to all maps by bpfd group members.
                self.save_map(id, map_prog_uuid, map_pin_path.clone())
                    .await?;

                Ok(id)
            }
            _ => panic!("not a supported single attach program"),
        }
    }

    pub(crate) async fn remove_program(
        &mut self,
        id: Uuid,
        owner: String,
    ) -> Result<(), BpfdError> {
        debug!("BpfManager::remove_program() id: {id}");
        if let Some(prog) = self.programs.get(&id) {
            if !(prog.owner() == &owner || owner == SUPERUSER) {
                return Err(BpfdError::NotAuthorized);
            }
            if !self.is_map_safe_to_delete(id, prog.data().map_prog_uuid.clone()) {
                return Err(BpfdError::Error(
                    "map being used by other eBPF program".to_string(),
                ));
            }
        } else {
            debug!("InvalidID: {id}");
            return Err(BpfdError::InvalidID);
        }

        let prog = self.programs.remove(&id).unwrap();

        let map_prog_uuid = prog.data().map_prog_uuid.clone();

        prog.delete(id)
            .map_err(|_| BpfdError::Error("unable to delete program data".to_string()))?;

        match prog {
            Program::Xdp(_) | Program::Tc(_) => self.remove_multi_attach_program(prog).await?,
            Program::Tracepoint(_) | Program::Uprobe(_) => (),
        }

        self.delete_map(id, map_prog_uuid).await?;
        Ok(())
    }

    pub(crate) async fn remove_multi_attach_program(
        &mut self,
        program: Program,
    ) -> Result<(), BpfdError> {
        debug!("BpfManager::remove_multi_attach_program()");
        // Calculate the next_available_id
        let next_available_id = self
            .programs
            .iter()
            .filter(|(_, p)| {
                if p.kind() == program.kind() {
                    p.if_index() == program.if_index() && p.direction() == program.direction()
                } else {
                    false
                }
            })
            .collect::<HashMap<_, _>>()
            .len();
        debug!("next_available_id = {next_available_id}");

        let did = program
            .dispatcher_id()
            .ok_or(BpfdError::DispatcherNotRequired)?;

        let mut old_dispatcher = self.dispatchers.remove(&did);

        if let Some(ref mut old) = old_dispatcher {
            if next_available_id == 0 {
                // Delete the dispatcher
                return old.delete(true);
            }
        }

        let program_type = program.kind();
        let if_index = program.if_index();
        let if_name = program.if_name().unwrap();
        let direction = program.direction();

        self.sort_programs(program_type, if_index, direction);

        let mut programs = self.collect_programs(program_type, if_index, direction);

        let if_config = if let Some(ref i) = self.config.interfaces {
            i.get(&if_name)
        } else {
            None
        };
        let next_revision = if let Some(ref old) = old_dispatcher {
            old.next_revision()
        } else {
            1
        };
        debug!("next_revision = {next_revision}");
        let dispatcher =
            Dispatcher::new(if_config, &mut programs, next_revision, old_dispatcher).await?;
        self.dispatchers.insert(did, dispatcher);
        Ok(())
    }

    pub(crate) async fn rebuild_multiattach_dispatcher(
        &mut self,
        program_type: ProgramType,
        if_index: u32,
        direction: Option<Direction>,
        did: DispatcherId,
    ) -> Result<(), BpfdError> {
        debug!("BpfManager::rebuild_multiattach_dispatcher() for program type {program_type} on if_index {if_index}");
        let mut old_dispatcher = self.dispatchers.remove(&did);

        if let Some(ref mut old) = old_dispatcher {
            debug!("Rebuild Multiattach Dispatcher for {did:?}");
            let if_index = Some(if_index);

            self.sort_programs(program_type, if_index, direction);
            let mut programs = self.collect_programs(program_type, if_index, direction);

            debug!("programs loaded: {}", programs.len());

            // The following checks should have been done when the dispatcher was built, but check again to confirm
            if programs.is_empty() {
                return old.delete(true);
            } else if programs.len() > 10 {
                return Err(BpfdError::TooManyPrograms);
            }

            let if_name = old.if_name();
            let if_config = if let Some(ref i) = self.config.interfaces {
                i.get(&if_name)
            } else {
                None
            };

            let next_revision = if let Some(ref old) = old_dispatcher {
                old.next_revision()
            } else {
                1
            };

            let dispatcher =
                Dispatcher::new(if_config, &mut programs, next_revision, old_dispatcher).await?;
            self.dispatchers.insert(did, dispatcher);
        } else {
            debug!("No dispatcher found in rebuild_multiattach_dispatcher() for {did:?}");
        }
        Ok(())
    }

    pub(crate) fn list_programs(&mut self) -> Result<Vec<ProgramInfo>, BpfdError> {
        debug!("BpfManager::list_programs()");

        let mut bpfd_progs: HashMap<u32, ProgramInfo> = self
            .programs
            .iter()
            .map(|(id, p)| {
                let location = Some(p.data().location.clone());
                let kernel_info = p
                    .data()
                    .kernel_info
                    .clone()
                    .expect("Loaded program should have kernel information");
                let prog_id = kernel_info.id;

                match p {
                    Program::Xdp(p) => (
                        prog_id,
                        ProgramInfo {
                            id: Some(*id),
                            name: Some(p.data.section_name.to_string()),
                            program_type: Some(ProgramType::Xdp as u32),
                            location,
                            global_data: Some(p.data.global_data.clone()),
                            map_prog_uuid: Some(p.data.map_prog_uuid.clone().unwrap_or_default()),
                            map_pin_path: Some(
                                self.get_map_pin_path(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            map_used_by: Some(
                                self.get_map_used_by(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            attach_info: Some(crate::command::AttachInfo::Xdp(
                                crate::command::XdpAttachInfo {
                                    iface: p.info.if_name.to_string(),
                                    priority: p.info.metadata.priority,
                                    proceed_on: p.info.proceed_on.clone(),
                                    position: p.info.current_position.unwrap_or_default() as i32,
                                },
                            )),
                            kernel_info,
                        },
                    ),
                    Program::Tracepoint(p) => (
                        prog_id,
                        ProgramInfo {
                            id: Some(*id),
                            name: Some(p.data.section_name.to_string()),
                            location,
                            program_type: Some(ProgramType::Tracepoint as u32),
                            global_data: Some(p.data.global_data.clone()),
                            map_prog_uuid: Some(p.data.map_prog_uuid.clone().unwrap_or_default()),
                            map_pin_path: Some(
                                self.get_map_pin_path(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            map_used_by: Some(
                                self.get_map_used_by(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            attach_info: Some(crate::command::AttachInfo::Tracepoint(
                                crate::command::TracepointAttachInfo {
                                    tracepoint: p.info.tracepoint.to_string(),
                                },
                            )),
                            kernel_info,
                        },
                    ),
                    Program::Tc(p) => (
                        prog_id,
                        ProgramInfo {
                            id: Some(*id),
                            name: Some(p.data.section_name.to_string()),
                            location,
                            program_type: Some(ProgramType::Tc as u32),
                            global_data: Some(p.data.global_data.clone()),
                            map_prog_uuid: Some(p.data.map_prog_uuid.clone().unwrap_or_default()),
                            map_pin_path: Some(
                                self.get_map_pin_path(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            map_used_by: Some(
                                self.get_map_used_by(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            attach_info: Some(crate::command::AttachInfo::Tc(
                                crate::command::TcAttachInfo {
                                    iface: p.info.if_name.to_string(),
                                    priority: p.info.metadata.priority,
                                    proceed_on: p.info.proceed_on.clone(),
                                    direction: p.direction,
                                    position: p.info.current_position.unwrap_or_default() as i32,
                                },
                            )),
                            kernel_info,
                        },
                    ),
                    Program::Uprobe(p) => (
                        prog_id,
                        ProgramInfo {
                            id: Some(*id),
                            name: Some(p.data.section_name.to_string()),
                            location,
                            program_type: Some(ProgramType::Kprobe as u32),
                            global_data: Some(p.data.global_data.clone()),
                            map_prog_uuid: Some(p.data.map_prog_uuid.clone().unwrap_or_default()),
                            map_pin_path: Some(
                                self.get_map_pin_path(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            map_used_by: Some(
                                self.get_map_used_by(*id, p.data.map_prog_uuid.clone())
                                    .unwrap_or_default(),
                            ),
                            attach_info: Some(crate::command::AttachInfo::Uprobe(
                                crate::command::UprobeAttachInfo {
                                    fn_name: p.info.fn_name.clone(),
                                    offset: p.info.offset,
                                    target: p.info.target.clone(),
                                    pid: p.info.pid,
                                    namespace: p.info.namespace.clone(),
                                },
                            )),
                            kernel_info,
                        },
                    ),
                }
            })
            .collect();

        loaded_programs()
            .map(|p| {
                let prog = p.map_err(BpfdError::BpfProgramError)?;
                let prog_id = prog.id();

                match bpfd_progs.remove(&prog_id) {
                    Some(p) => Ok(p),
                    None => Ok(ProgramInfo {
                        id: None,
                        name: None,
                        program_type: None,
                        location: None,
                        global_data: None,
                        map_prog_uuid: None,
                        map_pin_path: None,
                        map_used_by: None,
                        attach_info: None,
                        kernel_info: prog.try_into()?,
                    }),
                }
            })
            .collect()
    }

    fn sort_programs(
        &mut self,
        program_type: ProgramType,
        if_index: Option<u32>,
        direction: Option<Direction>,
    ) {
        let mut extensions = self
            .programs
            .iter_mut()
            .filter_map(|(k, v)| {
                if v.kind() == program_type {
                    if v.if_index() == if_index && v.direction() == direction {
                        Some((k, v))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(&Uuid, &mut Program)>>();
        extensions.sort_by(|(_, a), (_, b)| a.metadata().cmp(&b.metadata()));
        for (i, (_, v)) in extensions.iter_mut().enumerate() {
            v.set_position(Some(i));
        }
    }

    fn collect_programs(
        &self,
        program_type: ProgramType,
        if_index: Option<u32>,
        direction: Option<Direction>,
    ) -> Vec<(Uuid, Program)> {
        let mut results = vec![];
        for (k, v) in self.programs.iter() {
            if v.kind() == program_type && v.if_index() == if_index && v.direction() == direction {
                results.push((k.to_owned(), v.clone()))
            }
        }
        results
    }

    async fn pull_bytecode(&self, args: PullBytecodeArgs) -> anyhow::Result<()> {
        let res = match args.image.get_image(None).await {
            Ok(_) => {
                info!("Successfully pulled bytecode");
                Ok(())
            }
            Err(e) => Err(e).map_err(|e| BpfdError::BpfBytecodeError(e.into())),
        };

        let _ = args.responder.send(res);
        Ok(())
    }

    pub(crate) async fn process_commands(&mut self) {
        loop {
            // Start receiving messages
            select! {
                biased;
                _ = shutdown_handler() => {
                    info!("Signal received to stop command processing");
                    break;
                }
                Some(cmd) = self.commands.recv() => {
                    match cmd {
                        Command::LoadXDP(args) => self.load_xdp_command(args).await.unwrap(),
                        Command::LoadTC(args) => self.load_tc_command(args).await.unwrap(),
                        Command::LoadTracepoint(args) => self.load_tracepoint_command(args).await.unwrap(),
                        Command::LoadUprobe(args) => self.load_uprobe_command(args).await.unwrap(),
                        Command::Unload(args) => self.unload_command(args).await.unwrap(),
                        Command::List { responder } => {
                            let progs = self.list_programs();
                            // Ignore errors as they'll be propagated to caller in the RPC status
                            let _ = responder.send(progs);
                        }
                        Command::PullBytecode (args) => self.pull_bytecode(args).await.unwrap(),
                    }
                }
            }
        }
        info!("Stopping processing commands");
    }

    async fn load_xdp_command(&mut self, args: LoadXDPArgs) -> anyhow::Result<()> {
        let res = if let Ok(if_index) = get_ifindex(&args.iface) {
            match ProgramData::new(
                args.location,
                args.section_name.clone(),
                args.global_data,
                args.username,
                args.map_prog_uuid,
            )
            .await
            {
                Ok(prog_data) => {
                    let prog = Program::Xdp(XdpProgram {
                        data: prog_data.clone(),
                        info: XdpProgramInfo {
                            if_index,
                            current_position: None,
                            metadata: command::Metadata {
                                priority: args.priority,
                                // This could have been overridden by image tags
                                name: prog_data.section_name,
                                attached: false,
                            },
                            proceed_on: args.proceed_on,
                            if_name: args.iface,
                        },
                    });
                    self.add_program(prog, args.id).await
                }
                Err(e) => Err(e),
            }
        } else {
            Err(BpfdError::InvalidInterface)
        };

        // Ignore errors as they'll be propagated to caller in the RPC status
        let _ = args.responder.send(res);
        Ok(())
    }

    async fn load_tc_command(&mut self, args: LoadTCArgs) -> anyhow::Result<()> {
        let res = if let Ok(if_index) = get_ifindex(&args.iface) {
            match ProgramData::new(
                args.location,
                args.section_name,
                args.global_data,
                args.username,
                args.map_prog_uuid.clone(),
            )
            .await
            {
                Ok(prog_data) => {
                    let prog = Program::Tc(TcProgram {
                        data: prog_data.clone(),
                        direction: args.direction,
                        info: TcProgramInfo {
                            if_index,
                            current_position: None,
                            metadata: command::Metadata {
                                priority: args.priority,
                                // This could have been overridden by image tags
                                name: prog_data.section_name,
                                attached: false,
                            },
                            proceed_on: args.proceed_on,
                            if_name: args.iface,
                        },
                    });
                    self.add_program(prog, args.id).await
                }
                Err(e) => Err(e),
            }
        } else {
            Err(BpfdError::InvalidInterface)
        };

        // Ignore errors as they'll be propagated to caller in the RPC status
        let _ = args.responder.send(res);
        Ok(())
    }

    async fn load_tracepoint_command(&mut self, args: LoadTracepointArgs) -> anyhow::Result<()> {
        let res = {
            match ProgramData::new(
                args.location,
                args.section_name,
                args.global_data,
                args.username,
                args.map_prog_uuid.clone(),
            )
            .await
            {
                Ok(prog_data) => {
                    let prog = Program::Tracepoint(TracepointProgram {
                        data: prog_data,
                        info: TracepointProgramInfo {
                            tracepoint: args.tracepoint,
                        },
                    });
                    self.add_program(prog, args.id).await
                }
                Err(e) => Err(e),
            }
        };

        // Ignore errors as they'll be propagated to caller in the RPC status
        let _ = args.responder.send(res);
        Ok(())
    }

    async fn load_uprobe_command(&mut self, args: LoadUprobeArgs) -> anyhow::Result<()> {
        let res = {
            match ProgramData::new(
                args.location,
                args.section_name,
                args.global_data,
                args.username,
                args.map_prog_uuid.clone(),
            )
            .await
            {
                Ok(prog_data) => {
                    let prog = Program::Uprobe(UprobeProgram {
                        data: prog_data,
                        info: UprobeProgramInfo {
                            fn_name: args.fn_name,
                            offset: args.offset,
                            target: args.target,
                            pid: args.pid,
                            namespace: args._namespace,
                        },
                    });
                    self.add_program(prog, args.id).await
                }
                Err(e) => Err(e),
            }
        };

        // Ignore errors as they'll be propagated to caller in the RPC status
        let _ = args.responder.send(res);
        Ok(())
    }

    async fn unload_command(&mut self, args: UnloadArgs) -> anyhow::Result<()> {
        let res = self.remove_program(args.id, args.username).await;
        // Ignore errors as they'll be propagated to caller in the RPC status
        let _ = args.responder.send(res);
        Ok(())
    }

    // This function reads the map_pin_path from the map hash table. If there
    // is not an entry for the given input, an error is returned.
    fn get_map_pin_path(
        &self,
        id: Uuid,
        map_prog_uuid: Option<String>,
    ) -> Result<String, BpfdError> {
        let (_, map_index) = get_map_index(id, map_prog_uuid.clone());

        if let Some(map) = self.maps.get(&map_index) {
            Ok(map.map_pin_path.clone())
        } else {
            Err(BpfdError::Error("map does not exists".to_string()))
        }
    }

    // This function reads the map.used_by from the map hash table. If there
    // is not an entry for the given input, an error is returned. Internally,
    // the owner's UUID is also stored in the list. This function strips it out.
    fn get_map_used_by(
        &self,
        id: Uuid,
        map_prog_uuid: Option<String>,
    ) -> Result<Vec<String>, BpfdError> {
        let (map_owner, map_index) = get_map_index(id, map_prog_uuid.clone());

        if let Some(map) = self.maps.get(&map_index) {
            let map_owner_id = if map_owner {
                id.to_string()
            } else {
                map_prog_uuid.unwrap()
            };

            let mut used_by = map.used_by.clone();
            if let Some(index) = used_by
                .iter()
                .position(|value| *value == map_owner_id.clone())
            {
                used_by.swap_remove(index);
            }
            Ok(used_by)
        } else {
            Err(BpfdError::Error("map does not exists".to_string()))
        }
    }

    // This function returns the map_pin_path, and if this eBPF program is
    // the map owner, creates the directory to store the associate maps.
    async fn manage_map_pin_path(
        &mut self,
        id: Uuid,
        map_prog_uuid: Option<String>,
    ) -> Result<String, BpfdError> {
        let (map_owner, map_pin_path) = calc_map_pin_path(id, map_prog_uuid.clone());

        // If the user provided a UUID of an eBPF program to share a map with,
        // then use that UUID in the directory to create the maps in
        // (path already exists).
        // Otherwise, use the UUID of this program and create the directory.
        if map_owner {
            fs::create_dir_all(map_pin_path.clone())
                .await
                .map_err(|e| BpfdError::Error(format!("can't create map dir: {e}")))?;

            // Return the map_pin_path
            Ok(map_pin_path)
        } else {
            if self.maps.contains_key(&map_prog_uuid.clone().unwrap()) {
                // Return the map_pin_path
                return Ok(map_pin_path);
            }
            Err(BpfdError::Error(
                "map_prog_uuid does not exists".to_string(),
            ))
        }
    }

    // This function is called if manage_map_pin_path() was already called,
    // but the eBPF program failed to load. save_map() has not been called,
    // so self.maps has not been updated for this program.
    // If the user provided a UUID of program to share a map with,
    // then map the directory is still in use and there is nothing to do.
    // Otherwise, manage_map_pin_path() created the map directory so it must
    // deleted.
    async fn cleanup_map_pin_path(
        &mut self,
        id: Uuid,
        map_prog_uuid: Option<String>,
    ) -> Result<(), BpfdError> {
        let (map_owner, map_pin_path) = calc_map_pin_path(id, map_prog_uuid.clone());

        if map_owner {
            let _ = fs::remove_dir_all(map_pin_path.clone())
                .await
                .map_err(|e| BpfdError::Error(format!("can't delete map dir: {e}")));
            Ok(())
        } else {
            Ok(())
        }
    }

    // This function writes the map to the map hash table. If this eBPF
    // program is the map owner, then a new entry is add to the map hash
    // table and permissions on the directory are updated to grant bpfd
    // user group access to all the maps in the directory. If this eBPF
    // program is not the owner, then the eBPF program UUID is added to
    // the Used-By array.
    async fn save_map(
        &mut self,
        id: Uuid,
        map_prog_uuid: Option<String>,
        map_pin_path: String,
    ) -> Result<(), BpfdError> {
        let (map_owner, _) = get_map_index(id, map_prog_uuid.clone());

        if map_owner {
            let map = BpfMap {
                map_pin_path: map_pin_path.clone(),
                used_by: vec![id.to_string()],
            };

            self.maps.insert(id.to_string(), map);

            set_dir_permissions(&map_pin_path.clone(), MAPS_MODE).await;
        } else if let Some(map) = self.maps.get_mut(&map_prog_uuid.clone().unwrap()) {
            map.used_by.push(id.to_string());
        } else {
            return Err(BpfdError::Error(
                "map_prog_uuid does not exists".to_string(),
            ));
        };
        Ok(())
    }

    // This function is a pre-check to make sure the map can be deleted,
    // and thus the associated eBPF program can be unloaded. This function
    // returns false if this program is the map owner and other programs
    // are referencing the map, true otherwise.
    fn is_map_safe_to_delete(&mut self, id: Uuid, map_prog_uuid: Option<String>) -> bool {
        let (map_owner, _) = get_map_index(id, map_prog_uuid.clone());

        if map_owner {
            // If this eBPF program is eBPF program that created the map,
            // make sure no other eBPF programs are referencing the maps before
            // allowing it to be deleted.
            if let Some(map) = self.maps.get_mut(&id.to_string()) {
                if map.used_by.len() > 1 {
                    // USed by more than one eBPF program (one would be this program),
                    // so block the unload.
                    return false;
                }
            }
        }

        true
    }

    // This function cleans up a map entry when an eBPF program is
    // being unloaded. If the eBPF program is the map owner, then
    // the map is removed from the hash table and the associated
    // directory is removed. If this eBPF program is referencing a
    // map from another eBPF program, then this eBPF programs UUID
    // is removed from the UsedBy array.
    async fn delete_map(
        &mut self,
        id: Uuid,
        map_prog_uuid: Option<String>,
    ) -> Result<(), BpfdError> {
        let (_, map_index) = get_map_index(id, map_prog_uuid.clone());

        if let Some(map) = self.maps.get_mut(&map_index.clone()) {
            //for uuid in map.used_by.iter() {
            //    info!("  used_by: {:?}", uuid);
            //}
            if let Some(index) = map
                .used_by
                .iter()
                .position(|value| *value == id.to_string())
            {
                map.used_by.swap_remove(index);
            }

            if map.used_by.is_empty() {
                let (_, path) = calc_map_pin_path(id, map_prog_uuid.clone());
                self.maps.remove(&map_index.clone());
                fs::remove_dir_all(path)
                    .await
                    .map_err(|e| BpfdError::Error(format!("can't delete map dir: {e}")))?;
            }
        } else {
            return Err(BpfdError::Error("map_pin_path does not exists".to_string()));
        }

        Ok(())
    }

    fn rebuild_map_entry(&mut self, id: Uuid, map_prog_uuid: Option<String>) {
        let (_, map_index) = get_map_index(id, map_prog_uuid.clone());

        if let Some(map) = self.maps.get_mut(&map_index) {
            map.used_by.push(id.to_string());
        } else {
            let (_, map_pin_path) = calc_map_pin_path(id, map_prog_uuid.clone());
            let map = BpfMap {
                map_pin_path: map_pin_path.clone(),
                used_by: vec![id.to_string()],
            };
            self.maps.insert(id.to_string(), map);
        }
    }
}

// map_index is a UUID. It is either the programs UUID, or the UUID
// of another program that map_prog_uuid references.
// This function also returns a bool, which indicates if the input UUID
// is the owner of the map (map_prog_uuid is not set) or not (map_prog_uuid
// is set so the eBPF program is referencing another eBPF programs maps).
fn get_map_index(id: Uuid, map_prog_uuid: Option<String>) -> (bool, String) {
    if map_prog_uuid.is_none() || map_prog_uuid.clone().unwrap().is_empty() {
        (true, id.to_string())
    } else {
        (false, map_prog_uuid.clone().unwrap())
    }
}

// map_pin_path is a the directory the maps are located. Currently, it
// is a fixed bpfd location containing the map_index, which is a UUID.
// The UUID is either the programs UUID, or the UUID of another program
// that map_prog_uuid references.
pub fn calc_map_pin_path(id: Uuid, map_prog_uuid: Option<String>) -> (bool, String) {
    let (map_owner, map_index) = get_map_index(id, map_prog_uuid.clone());
    (map_owner, format!("{RTDIR_FS_MAPS}/{map_index}"))
}
