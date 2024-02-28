// SPDX-License-Identifier: Apache-2.0
// Copyright Authors of bpfman

use bpfman_api::v1::{KernelProgramInfo, ProgramInfo};
use log::warn;

use crate::{
    bpf::BpfManager,
    cli::{args::GetArgs, table::ProgTable},
    command::Program,
    errors::BpfmanError,
};

pub(crate) fn execute_get(bpf_manager: &mut BpfManager, args: &GetArgs) -> Result<(), BpfmanError> {
    match bpf_manager.get_program(args.id) {
        Ok(program) => {
            let info: Option<ProgramInfo> = if let Program::Unsupported(_) = program {
                None
            } else {
                Some((&program).try_into()?)
            };
            let kernel_info: Option<KernelProgramInfo> = match (&program).try_into() {
                Ok(i) => {
                    if let Program::Unsupported(_) = program {
                        program.delete()?
                    };
                    Some(i)
                }
                Err(e) => return Err(e),
            };

            ProgTable::new_get_bpfman(&info)?.print();
            ProgTable::new_get_unsupported(&kernel_info)?.print();
            Ok(())
        }
        Err(e) => {
            warn!("BPFMAN get error: {}", e);
            Err(e)
        }
    }
}
