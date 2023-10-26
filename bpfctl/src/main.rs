// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright Authors of bpfd

use std::{collections::HashMap, fs, net::SocketAddr, str};

use anyhow::{bail, Context};
use base64::{engine::general_purpose, Engine as _};
use bpfd_api::{
    config::{self, Config},
    util::directories::*,
    v1::{
        attach_info::Info, bpfd_client::BpfdClient, bytecode_location::Location,
        list_response::ListResult, AttachInfo, BytecodeImage, BytecodeLocation, GetRequest,
        KernelProgramInfo, KprobeAttachInfo, ListRequest, LoadRequest, ProgramInfo,
        PullBytecodeRequest, TcAttachInfo, TracepointAttachInfo, UnloadRequest, UprobeAttachInfo,
        XdpAttachInfo,
    },
    ImagePullPolicy,
    ProbeType::*,
    ProgramType, TcProceedOn, XdpProceedOn,
};
use clap::{Args, Parser, Subcommand};
use comfy_table::{Cell, Color, Table};
use hex::{encode_upper, FromHex};
use log::{info, warn};
use tokio::net::UnixStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Uri};
use tower::service_fn;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Load an eBPF program from a local .o file.
    LoadFromFile(LoadFileArgs),
    /// Load an eBPF program packaged in a OCI container image from a given registry.
    LoadFromImage(LoadImageArgs),
    /// Unload an eBPF program using the program id.
    Unload(UnloadArgs),
    /// List all eBPF programs loaded via bpfd.
    List(ListArgs),
    /// Get an eBPF program using the program id.
    Get {
        /// Required: Program id to be retrieved.
        id: u32,
    },
    /// Pull a bytecode image for future use by a load command.
    PullBytecode(PullBytecodeArgs),
}

#[derive(Args)]
struct ListArgs {
    /// Optional: List a specific program type
    /// Example: --program-type xdp
    ///
    /// [possible values: unspec, socket-filter, kprobe, tc, sched-act,
    ///                   tracepoint, xdp, perf-event, cgroup-skb,
    ///                   cgroup-sock, lwt-in, lwt-out, lwt-xmit, sock-ops,
    ///                   sk-skb, cgroup-device, sk-msg, raw-tracepoint,
    ///                   cgroup-sock-addr, lwt-seg6-local, lirc-mode2,
    ///                   sk-reuseport, flow-dissector, cgroup-sysctl,
    ///                   raw-tracepoint-writable, cgroup-sockopt, tracing,
    ///                   struct-ops, ext, lsm, sk-lookup, syscall]
    #[clap(short, long, verbatim_doc_comment, hide_possible_values = true)]
    program_type: Option<ProgramType>,

    /// Optional: List programs which contain a specific set of metadata labels
    /// that were applied when the program was loaded with `--metadata` parameter.
    /// Format: <KEY>=<VALUE>
    ///
    /// Example: --metadata-selector owner=acme
    #[clap(short, long, verbatim_doc_comment, value_parser=parse_key_val, value_delimiter = ',')]
    metadata_selector: Option<Vec<(String, String)>>,

    /// Optional: List all programs.
    #[clap(short, long, verbatim_doc_comment)]
    all: bool,

    /// Optional: List all bpfd programs attached to a given network interface.
    /// Example: --interface eth0
    #[clap(short, long, verbatim_doc_comment)]
    interface: Option<String>,
}

#[derive(Args)]
struct LoadFileArgs {
    /// Required: Location of local bytecode file
    /// Example: --path /run/bpfd/examples/go-xdp-counter/bpf_bpfel.o
    #[clap(short, long, verbatim_doc_comment)]
    path: String,

    /// Required: The name of the function that is the entry point for the BPF program.
    #[clap(short, long)]
    name: String,

    /// Optional: Global variables to be set when program is loaded.
    /// Format: <NAME>=<Hex Value>
    ///
    /// This is a very low level primitive. The caller is responsible for formatting
    /// the byte string appropriately considering such things as size, endianness,
    /// alignment and packing of data structures.
    #[clap(short, long, verbatim_doc_comment, num_args(1..), value_parser=parse_global_arg)]
    global: Option<Vec<GlobalArg>>,

    /// Optional: Specify Key/Value metadata to be attached to a program when it
    /// is loaded by bpfd.
    /// Format: <KEY>=<VALUE>
    ///
    /// This can later be used to `list` a certain subset of programs which contain
    /// the specified metadata.
    /// Example: --metadata owner=acme
    #[clap(short, long, verbatim_doc_comment, value_parser=parse_key_val, value_delimiter = ',')]
    metadata: Option<Vec<(String, String)>>,

    /// Optional: Program id of loaded eBPF program this eBPF program will share a map with.
    /// Only used when multiple eBPF programs need to share a map.
    /// Example: --map-owner-id 63178
    #[clap(long, verbatim_doc_comment)]
    map_owner_id: Option<u32>,

    #[clap(subcommand)]
    command: LoadCommands,
}

#[derive(Args)]
struct LoadImageArgs {
    /// Specify how the bytecode image should be pulled.
    #[command(flatten)]
    pull_args: PullBytecodeArgs,

    /// Optional: The name of the function that is the entry point for the BPF program.
    /// If not provided, the program name defined as part of the bytecode image will be used.
    #[clap(short, long, verbatim_doc_comment, default_value = "")]
    name: String,

    /// Optional: Global variables to be set when program is loaded.
    /// Format: <NAME>=<Hex Value>
    ///
    /// This is a very low level primitive. The caller is responsible for formatting
    /// the byte string appropriately considering such things as size, endianness,
    /// alignment and packing of data structures.
    #[clap(short, long, verbatim_doc_comment, num_args(1..), value_parser=parse_global_arg)]
    global: Option<Vec<GlobalArg>>,

    /// Optional: Specify Key/Value metadata to be attached to a program when it
    /// is loaded by bpfd.
    /// Format: <KEY>=<VALUE>
    ///
    /// This can later be used to list a certain subset of programs which contain
    /// the specified metadata.
    /// Example: --metadata owner=acme
    #[clap(short, long, verbatim_doc_comment, value_parser=parse_key_val, value_delimiter = ',')]
    metadata: Option<Vec<(String, String)>>,

    /// Optional: Program id of loaded eBPF program this eBPF program will share a map with.
    /// Only used when multiple eBPF programs need to share a map.
    /// Example: --map-owner-id 63178
    #[clap(long, verbatim_doc_comment)]
    map_owner_id: Option<u32>,

    #[clap(subcommand)]
    command: LoadCommands,
}

#[derive(Subcommand)]
enum LoadCommands {
    /// Install an eBPF program on the XDP hook point for a given interface.
    Xdp {
        /// Required: Interface to load program on.
        #[clap(short, long)]
        iface: String,

        /// Required: Priority to run program in chain. Lower value runs first.
        #[clap(short, long)]
        priority: i32,

        /// Optional: Proceed to call other programs in chain on this exit code.
        /// Multiple values supported by repeating the parameter.
        /// Example: --proceed-on "pass" --proceed-on "drop"
        ///
        /// [possible values: aborted, drop, pass, tx, redirect, dispatcher_return]
        ///
        /// [default: pass, dispatcher_return]
        #[clap(long, verbatim_doc_comment, num_args(1..))]
        proceed_on: Vec<String>,
    },
    /// Install an eBPF program on the TC hook point for a given interface.
    Tc {
        /// Required: Direction to apply program.
        ///
        /// [possible values: ingress, egress]
        #[clap(short, long, verbatim_doc_comment)]
        direction: String,

        /// Required: Interface to load program on.
        #[clap(short, long)]
        iface: String,

        /// Required: Priority to run program in chain. Lower value runs first.
        #[clap(short, long)]
        priority: i32,

        /// Optional: Proceed to call other programs in chain on this exit code.
        /// Multiple values supported by repeating the parameter.
        /// Example: --proceed-on "ok" --proceed-on "pipe"
        ///
        /// [possible values: unspec, ok, reclassify, shot, pipe, stolen, queued,
        ///                   repeat, redirect, trap, dispatcher_return]
        ///
        /// [default: ok, pipe, dispatcher_return]
        #[clap(long, verbatim_doc_comment, num_args(1..))]
        proceed_on: Vec<String>,
    },
    /// Install an eBPF program on a Tracepoint.
    Tracepoint {
        /// Required: The tracepoint to attach to.
        /// Example: --tracepoint "sched/sched_switch"
        #[clap(short, long, verbatim_doc_comment)]
        tracepoint: String,
    },
    /// Install an eBPF kprobe or kretprobe
    Kprobe {
        /// Required: Function to attach the kprobe to.
        #[clap(short, long)]
        fn_name: String,

        /// Optional: Offset added to the address of the function for kprobe.
        /// Not allowed for kretprobes.
        #[clap(short, long, verbatim_doc_comment)]
        offset: Option<u64>,

        /// Optional: Whether the program is a kretprobe.
        ///
        /// [default: false]
        #[clap(short, long, verbatim_doc_comment)]
        retprobe: bool,

        /// Optional: Namespace to attach the kprobe in. (NOT CURRENTLY SUPPORTED)
        #[clap(short, long)]
        namespace: Option<String>,
    },
    /// Install an eBPF uprobe or uretprobe
    Uprobe {
        /// Optional: Function to attach the uprobe to.
        #[clap(short, long)]
        fn_name: Option<String>,

        /// Optional: Offset added to the address of the target function (or
        /// beginning of target if no function is identified). Offsets are
        /// supported for uretprobes, but use with caution because they can
        /// result in unintended side effects.
        #[clap(short, long, verbatim_doc_comment)]
        offset: Option<u64>,

        /// Required: Library name or the absolute path to a binary or library.
        /// Example: --target "libc".
        #[clap(short, long, verbatim_doc_comment)]
        target: String,

        /// Optional: Whether the program is a uretprobe.
        ///
        /// [default: false]
        #[clap(short, long, verbatim_doc_comment)]
        retprobe: bool,

        /// Optional: Only execute uprobe for given process identification number (PID).
        /// If PID is not provided, uprobe executes for all PIDs.
        #[clap(short, long, verbatim_doc_comment)]
        pid: Option<i32>,

        /// Optional: Namespace to attach the uprobe in. (NOT CURRENTLY SUPPORTED)
        #[clap(short, long)]
        namespace: Option<String>,
    },
}

#[derive(Args)]
struct UnloadArgs {
    /// Required: Program id to be unloaded.
    id: u32,
}

#[derive(Args)]
struct PullBytecodeArgs {
    /// Required: Container Image URL.
    /// Example: --image-url quay.io/bpfd-bytecode/xdp_pass:latest
    #[clap(short, long, verbatim_doc_comment)]
    image_url: String,

    /// Optional: Registry auth for authenticating with the specified image registry.
    /// This should be base64 encoded from the '<username>:<password>' string just like
    /// it's stored in the docker/podman host config.
    /// Example: --registry_auth "YnjrcKw63PhDcQodiU9hYxQ2"
    #[clap(short, long, verbatim_doc_comment)]
    registry_auth: Option<String>,

    /// Optional: Pull policy for remote images.
    ///
    /// [possible values: Always, IfNotPresent, Never]
    #[clap(short, long, verbatim_doc_comment, default_value = "IfNotPresent")]
    pull_policy: String,
}

impl TryFrom<&PullBytecodeArgs> for BytecodeImage {
    type Error = anyhow::Error;

    fn try_from(value: &PullBytecodeArgs) -> Result<Self, Self::Error> {
        let pull_policy: ImagePullPolicy = value.pull_policy.as_str().try_into()?;
        let (username, password) = match &value.registry_auth {
            Some(a) => {
                let auth_raw = general_purpose::STANDARD.decode(a)?;
                let auth_string = String::from_utf8(auth_raw)?;
                let (username, password) = auth_string.split_once(':').unwrap();
                (username.to_owned(), password.to_owned())
            }
            None => ("".to_owned(), "".to_owned()),
        };

        Ok(BytecodeImage {
            url: value.image_url.clone(),
            image_pull_policy: pull_policy.into(),
            username: Some(username),
            password: Some(password),
        })
    }
}

#[derive(Clone, Debug)]
struct GlobalArg {
    name: String,
    value: Vec<u8>,
}

struct ProgTable(Table);

impl ProgTable {
    fn new_get_bpfd(r: &Option<ProgramInfo>) -> Result<Self, anyhow::Error> {
        let mut table = Table::new();

        table.load_preset(comfy_table::presets::NOTHING);
        table.set_header(vec![Cell::new("Bpfd State")
            .add_attribute(comfy_table::Attribute::Bold)
            .add_attribute(comfy_table::Attribute::Underlined)
            .fg(Color::Green)]);

        if r.is_none() {
            table.add_row(vec!["NONE"]);
            return Ok(ProgTable(table));
        }
        let info = r.clone().unwrap();

        if info.bytecode.is_none() {
            table.add_row(vec!["NONE"]);
            return Ok(ProgTable(table));
        }

        if info.name.clone().is_empty() {
            table.add_row(vec!["Name:", "None"]);
        } else {
            table.add_row(vec!["Name:", &info.name.clone()]);
        }

        match info.bytecode.clone().unwrap().location.clone() {
            Some(l) => match l {
                Location::Image(i) => {
                    table.add_row(vec!["Image URL:", &i.url]);
                    table.add_row(vec!["Pull Policy:", &format!{ "{}", TryInto::<ImagePullPolicy>::try_into(i.image_pull_policy)?}]);
                }
                Location::File(p) => {
                    table.add_row(vec!["Path:", &p]);
                }
            },
            // not a bpfd program
            None => {
                table.add_row(vec!["NONE"]);
                return Ok(ProgTable(table));
            }
        }

        if info.global_data.is_empty() {
            table.add_row(vec!["Global:", "None"]);
        } else {
            let mut first = true;
            for (key, value) in info.global_data.clone() {
                let data = &format! {"{key}={}", encode_upper(value)};
                if first {
                    first = false;
                    table.add_row(vec!["Global:", data]);
                } else {
                    table.add_row(vec!["", data]);
                }
            }
        }

        if info.metadata.is_empty() {
            table.add_row(vec!["Metadata:", "None"]);
        } else {
            let mut first = true;
            for (key, value) in info.metadata.clone() {
                let data = &format! {"{key}={value}"};
                if first {
                    first = false;
                    table.add_row(vec!["Metadata:", data]);
                } else {
                    table.add_row(vec!["", data]);
                }
            }
        }

        if info.map_pin_path.clone().is_empty() {
            table.add_row(vec!["Map Pin Path:", "None"]);
        } else {
            table.add_row(vec!["Map Pin Path:", &info.map_pin_path.clone()]);
        }

        match info.map_owner_id {
            Some(id) => table.add_row(vec!["Map Owner ID:", &id.to_string()]),
            None => table.add_row(vec!["Map Owner ID:", "None"]),
        };

        if info.map_used_by.clone().is_empty() {
            table.add_row(vec!["Maps Used By:", "None"]);
        } else {
            let mut first = true;
            for prog_id in info.clone().map_used_by {
                if first {
                    first = false;
                    table.add_row(vec!["Maps Used By:", &prog_id]);
                } else {
                    table.add_row(vec!["", &prog_id]);
                }
            }
        };

        if info.attach.is_some() {
            match info.attach.clone().unwrap().info.unwrap() {
                Info::XdpAttachInfo(XdpAttachInfo {
                    priority,
                    iface,
                    position,
                    proceed_on,
                }) => {
                    let proc_on = match XdpProceedOn::from_int32s(proceed_on) {
                        Ok(p) => p,
                        Err(e) => bail!("error parsing proceed_on {e}"),
                    };

                    table.add_row(vec!["Priority:", &priority.to_string()]);
                    table.add_row(vec!["Iface:", &iface]);
                    table.add_row(vec!["Position:", &position.to_string()]);
                    table.add_row(vec!["Proceed On:", &format!("{proc_on}")]);
                }
                Info::TcAttachInfo(TcAttachInfo {
                    priority,
                    iface,
                    position,
                    direction,
                    proceed_on,
                }) => {
                    let proc_on = match TcProceedOn::from_int32s(proceed_on) {
                        Ok(p) => p,
                        Err(e) => bail!("error parsing proceed_on {e}"),
                    };

                    table.add_row(vec!["Priority:", &priority.to_string()]);
                    table.add_row(vec!["Iface:", &iface]);
                    table.add_row(vec!["Position:", &position.to_string()]);
                    table.add_row(vec!["Direction:", &direction]);
                    table.add_row(vec!["Proceed On:", &format!("{proc_on}")]);
                }
                Info::TracepointAttachInfo(TracepointAttachInfo { tracepoint }) => {
                    table.add_row(vec!["Tracepoint:", &tracepoint]);
                }
                Info::KprobeAttachInfo(KprobeAttachInfo {
                    fn_name,
                    offset,
                    retprobe,
                    namespace,
                }) => {
                    let probe_type = match retprobe {
                        true => Kretprobe,
                        false => Kprobe,
                    };

                    table.add_row(vec!["Probe Type:", &format!["{probe_type}"]]);
                    table.add_row(vec!["Function Name:", &fn_name]);
                    table.add_row(vec!["Offset:", &offset.to_string()]);
                    table.add_row(vec!["Namespace", &namespace.unwrap_or("".to_string())]);
                }
                Info::UprobeAttachInfo(UprobeAttachInfo {
                    fn_name,
                    offset,
                    target,
                    retprobe,
                    pid,
                    namespace,
                }) => {
                    let probe_type = match retprobe {
                        true => Uretprobe,
                        false => Uprobe,
                    };

                    table.add_row(vec!["Probe Type:", &format!["{probe_type}"]]);
                    table.add_row(vec!["Function Name:", &fn_name.unwrap_or("".to_string())]);
                    table.add_row(vec!["Offset:", &offset.to_string()]);
                    table.add_row(vec!["Target:", &target]);
                    table.add_row(vec!["PID", &pid.unwrap_or(0).to_string()]);
                    table.add_row(vec!["Namespace", &namespace.unwrap_or("".to_string())]);
                }
            }
        }

        Ok(ProgTable(table))
    }

    fn new_get_unsupported(r: &Option<KernelProgramInfo>) -> Result<Self, anyhow::Error> {
        let mut table = Table::new();

        table.load_preset(comfy_table::presets::NOTHING);
        table.set_header(vec![Cell::new("Kernel State")
            .add_attribute(comfy_table::Attribute::Bold)
            .add_attribute(comfy_table::Attribute::Underlined)
            .fg(Color::Green)]);

        if r.is_none() {
            table.add_row(vec!["NONE"]);
            return Ok(ProgTable(table));
        }
        let kernel_info = r.clone().unwrap();

        let name = if kernel_info.name.clone().is_empty() {
            "None".to_string()
        } else {
            kernel_info.name.clone()
        };

        let rows = vec![
            vec!["ID:".to_string(), kernel_info.id.to_string()],
            vec!["Name:".to_string(), name],
            vec![
                "Type:".to_string(),
                format!("{}", ProgramType::try_from(kernel_info.program_type)?),
            ],
            vec!["Loaded At:".to_string(), kernel_info.loaded_at.clone()],
            vec!["Tag:".to_string(), kernel_info.tag.clone()],
            vec![
                "GPL Compatible:".to_string(),
                kernel_info.gpl_compatible.to_string(),
            ],
            vec!["Map IDs:".to_string(), format!("{:?}", kernel_info.map_ids)],
            vec!["BTF ID:".to_string(), kernel_info.btf_id.to_string()],
            vec![
                "Size Translated (bytes):".to_string(),
                kernel_info.bytes_xlated.to_string(),
            ],
            vec!["JITted:".to_string(), kernel_info.jited.to_string()],
            vec![
                "Size JITted:".to_string(),
                kernel_info.bytes_jited.to_string(),
            ],
            vec![
                "Kernel Allocated Memory (bytes):".to_string(),
                kernel_info.bytes_memlock.to_string(),
            ],
            vec![
                "Verified Instruction Count:".to_string(),
                kernel_info.verified_insns.to_string(),
            ],
        ];
        table.add_rows(rows);

        Ok(ProgTable(table))
    }
    
    fn new_list_xdp(&mut self) {
        let mut table = Table::new();

        table.load_preset(comfy_table::presets::NOTHING);
        vec![Cell::new("XDP Programs")
        .add_attribute(comfy_table::Attribute::Bold)
        .add_attribute(comfy_table::Attribute::Underlined)
        .fg(Color::Green)];

        table.add_row(vec!["Program ID", "Priority", "Name", "Interface", "Proceed On"]);
    }

    fn add_row_list_xdp(&mut self, id: String, priority: u32,  name: String, interface: String, proceed_on: Vec<String>) {
        let mut pro_on_str = String::new();

        proceed_on.into_iter().for_each(|x| pro_on_str.push_str(format!("{x},").as_str()));
        self.0.add_row(vec![id, name, interface, pro_on_str]);
    }

    fn new_list_tc(&mut self) {
        let mut table = Table::new();

        table.set_header(vec![Cell::new("TC Programs")
        .add_attribute(comfy_table::Attribute::Bold)
        .add_attribute(comfy_table::Attribute::Underlined)
        .fg(Color::Green)]);

        table.load_preset(comfy_table::presets::NOTHING);
        table.add_row(vec!["Program ID", "Priority", "Name", "Interface", "Direction", "Proceed On"]);
    }

    fn add_row_list_tc(&mut self, id: String, priority: u32, name: String, interface: String, direction: String, proceed_on: Vec<String>) {
        let mut pro_on_str = String::new();

        proceed_on.into_iter().for_each(|x| pro_on_str.push_str(format!("{x},").as_str()));
        self.0.add_row(vec![id, name, interface, direction, pro_on_str]);
    }

    fn new_list_tracepoint(&mut self) {
        let mut table = Table::new();

        table.set_header(vec![Cell::new("Tracepoint Programs")
        .add_attribute(comfy_table::Attribute::Bold)
        .add_attribute(comfy_table::Attribute::Underlined)
        .fg(Color::Green)]);

        table.load_preset(comfy_table::presets::NOTHING);
        table.set_header(vec!["Program ID", "Name", "Tracepoint"]);
        ProgTable(table)
    }

    fn add_row_list_tracepoint(&mut self, id: String, name: String, tracepoint: String) {
        self.0.add_row(vec![id, name, tracepoint]);
    }

    fn new_list_uprobe(&mut self) {
        let mut table = Table::new();

        table.set_header(vec![Cell::new("Uprobe Programs")
        .add_attribute(comfy_table::Attribute::Bold)
        .add_attribute(comfy_table::Attribute::Underlined)
        .fg(Color::Green)]);

        table.load_preset(comfy_table::presets::NOTHING);
        table.set_header(vec!["Program ID", "Name", "Function_name", "Offset", "Target", "Retprobe", "Pid"]);
        ProgTable(table)
    }

    fn add_row_list_uprobe(&mut self, id: String, name: String, fn_name: String, offset: String, target: String, retprobe: String, pid: Option<String>) {
        self.0.add_row(vec![id, name, fn_name, offset, target, retprobe, pid.unwrap_or("None".to_string())]);
    }

    fn new_list_kprobe(&mut self) {
        let mut table = Table::new();

        table.set_header(vec![Cell::new("Kprobe Programs")
        .add_attribute(comfy_table::Attribute::Bold)
        .add_attribute(comfy_table::Attribute::Underlined)
        .fg(Color::Green)]);

        table.load_preset(comfy_table::presets::NOTHING);
        table.set_header(vec!["Program ID", "Name", "Function_name", "Offset", "Retprobe"]);
        ProgTable(table)
    }

    fn add_row_list_kprobe(&mut self, id: String, name: String, fn_name: String, offset: String, retprobe: String) {
        self.0.add_row(vec![id, name, fn_name, offset, retprobe]);
    }

    fn new_list_all(&mut self, type_: String) {
        let mut table = Table::new();

        table.set_header(vec![Cell::new(format!("{} Programs", type_))
        .add_attribute(comfy_table::Attribute::Bold)
        .add_attribute(comfy_table::Attribute::Underlined)
        .fg(Color::Green)]);

        table.load_preset(comfy_table::presets::NOTHING);
        table.set_header(vec!["Program ID", "Name", "Load Time"]);
        ProgTable(table)
    }

    
    fn add_row_list_all(&mut self, id: String, name: String, type_: String, load_time: String) {
        self.0.add_row(vec![id, name, type_, load_time]);
    }

    fn print(&self) {
        println!("{self}\n")
    }
}

impl std::fmt::Display for ProgTable {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl LoadCommands {
    fn get_prog_type(&self) -> ProgramType {
        match self {
            LoadCommands::Xdp { .. } => ProgramType::Xdp,
            LoadCommands::Tc { .. } => ProgramType::Tc,
            LoadCommands::Tracepoint { .. } => ProgramType::Tracepoint,
            LoadCommands::Kprobe { .. } => ProgramType::Probe,
            LoadCommands::Uprobe { .. } => ProgramType::Probe,
        }
    }

    fn get_attach_type(&self) -> Result<Option<AttachInfo>, anyhow::Error> {
        match self {
            LoadCommands::Xdp {
                iface,
                priority,
                proceed_on,
            } => {
                let proc_on = match XdpProceedOn::from_strings(proceed_on) {
                    Ok(p) => p,
                    Err(e) => bail!("error parsing proceed_on {e}"),
                };
                Ok(Some(AttachInfo {
                    info: Some(Info::XdpAttachInfo(XdpAttachInfo {
                        priority: *priority,
                        iface: iface.to_string(),
                        position: 0,
                        proceed_on: proc_on.as_action_vec(),
                    })),
                }))
            }
            LoadCommands::Tc {
                direction,
                iface,
                priority,
                proceed_on,
            } => {
                match direction.as_str() {
                    "ingress" | "egress" => (),
                    other => bail!("{} is not a valid direction", other),
                };
                let proc_on = match TcProceedOn::from_strings(proceed_on) {
                    Ok(p) => p,
                    Err(e) => bail!("error parsing proceed_on {e}"),
                };
                Ok(Some(AttachInfo {
                    info: Some(Info::TcAttachInfo(TcAttachInfo {
                        priority: *priority,
                        iface: iface.to_string(),
                        position: 0,
                        direction: direction.to_string(),
                        proceed_on: proc_on.as_action_vec(),
                    })),
                }))
            }
            LoadCommands::Tracepoint { tracepoint } => Ok(Some(AttachInfo {
                info: Some(Info::TracepointAttachInfo(TracepointAttachInfo {
                    tracepoint: tracepoint.to_string(),
                })),
            })),
            LoadCommands::Kprobe {
                fn_name,
                offset,
                retprobe,
                namespace,
            } => {
                if namespace.is_some() {
                    bail!("kprobe namespace option not supported yet");
                }
                let offset = offset.unwrap_or(0);
                Ok(Some(AttachInfo {
                    info: Some(Info::KprobeAttachInfo(KprobeAttachInfo {
                        fn_name: fn_name.to_string(),
                        offset,
                        retprobe: *retprobe,
                        namespace: namespace.clone(),
                    })),
                }))
            }
            LoadCommands::Uprobe {
                fn_name,
                offset,
                target,
                retprobe,
                pid,
                namespace,
            } => {
                if namespace.is_some() {
                    bail!("uprobe namespace option not supported yet");
                }
                let offset = offset.unwrap_or(0);
                Ok(Some(AttachInfo {
                    info: Some(Info::UprobeAttachInfo(UprobeAttachInfo {
                        fn_name: fn_name.clone(),
                        offset,
                        target: target.clone(),
                        retprobe: *retprobe,
                        pid: *pid,
                        namespace: namespace.clone(),
                    })),
                }))
            }
        }
    }
}

impl Commands {
    fn get_bytecode_location(&self) -> anyhow::Result<Option<BytecodeLocation>> {
        match self {
            Commands::LoadFromFile(LoadFileArgs { path, .. }) => Ok(Some(BytecodeLocation {
                location: Some(Location::File(path.clone())),
            })),
            Commands::LoadFromImage(LoadImageArgs { pull_args, .. }) => {
                Ok(Some(BytecodeLocation {
                    location: Some(Location::Image(pull_args.try_into()?)),
                }))
            }
            _ => bail!("Unknown Command"),
        }
    }

    fn get_attach_info(&self) -> anyhow::Result<Option<AttachInfo>> {
        match self {
            Commands::LoadFromFile(l) => l.command.get_attach_type(),
            Commands::LoadFromImage(l) => l.command.get_attach_type(),
            _ => bail!("Unknown command"),
        }
    }
}

/// Parse a single key-value pair
fn parse_key_val(s: &str) -> Result<(String, String), std::io::Error> {
    let pos = s.find('=').ok_or(std::io::ErrorKind::InvalidInput)?;
    Ok((s[..pos].to_string(), s[pos + 1..].to_string()))
}

fn parse_global(global: &Option<Vec<GlobalArg>>) -> HashMap<String, Vec<u8>> {
    let mut global_data: HashMap<String, Vec<u8>> = HashMap::new();

    if let Some(global) = global {
        for g in global.iter() {
            global_data.insert(g.name.to_string(), g.value.clone());
        }
    }

    global_data
}

fn parse_global_arg(global_arg: &str) -> Result<GlobalArg, std::io::Error> {
    let mut parts = global_arg.split('=');

    let name_str = parts.next().ok_or(std::io::ErrorKind::InvalidInput)?;

    let value_str = parts.next().ok_or(std::io::ErrorKind::InvalidInput)?;
    let value = Vec::<u8>::from_hex(value_str).map_err(|_e| std::io::ErrorKind::InvalidInput)?;
    if value.is_empty() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }

    Ok(GlobalArg {
        name: name_str.to_string(),
        value,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // For output to bpfctl commands, eprintln() should be used. This includes
    // errors returned from bpfd. Every command should print some success indication
    // or a meaningful error.
    // logs (warn!(), info!(), debug!()) can be used by developers to help debug
    // failure cases. Being a CLI, they will be limited in their use. To see logs
    // for bpfctl commands, use the RUST_LOG environment variable:
    //    $ RUST_LOG=info bpfctl list
    env_logger::init();

    let cli = Cli::parse();

    let config = if let Ok(c) = fs::read_to_string(CFGPATH_BPFD_CONFIG) {
        c.parse().unwrap_or_else(|_| {
            warn!("Unable to parse config file, using defaults");
            Config::default()
        })
    } else {
        warn!("Unable to read config file, using defaults");
        Config::default()
    };

    let ca_cert = tokio::fs::read(&config.tls.ca_cert)
        .await
        .context("CA Cert File does not exist")?;
    let ca_cert = Certificate::from_pem(ca_cert);
    let cert = tokio::fs::read(&config.tls.client_cert)
        .await
        .context("Cert File does not exist")?;
    let key = tokio::fs::read(&config.tls.client_key)
        .await
        .context("Cert Key File does not exist")?;
    let identity = Identity::from_pem(cert, key);
    let tls_config = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(ca_cert)
        .identity(identity);

    for endpoint in config.grpc.endpoints {
        match endpoint {
            config::Endpoint::Tcp {
                address,
                port,
                enabled,
            } if !enabled => info!("Skipping disabled endpoint on {address}, port: {port}"),
            config::Endpoint::Tcp {
                address,
                port,
                enabled: _,
            } => match execute_request_tcp(&cli.command, address, port, tls_config.clone()).await {
                Ok(_) => return Ok(()),
                Err(e) => eprintln!("Error = {e:?}"),
            },
            config::Endpoint::Unix { path, enabled } if !enabled => {
                info!("Skipping disabled endpoint on {path}")
            }
            config::Endpoint::Unix { path, enabled: _ } => {
                match execute_request_unix(&cli.command, path).await {
                    Ok(_) => return Ok(()),
                    Err(e) => eprintln!("Error = {e:?}"),
                }
            }
        }
    }
    bail!("Failed to execute request")
}

async fn execute_request_unix(command: &Commands, path: String) -> anyhow::Result<()> {
    // Format address to something like: "unix://run/bpfd/bpfd.sock"
    let address = format!("unix:/{path}");
    let channel = Endpoint::try_from(address)?
        .connect_with_connector(service_fn(move |_: Uri| UnixStream::connect(path.clone())))
        .await?;

    info!("Using UNIX socket as transport");
    execute_request(command, channel).await
}

async fn execute_request_tcp(
    command: &Commands,
    address: String,
    port: u16,
    tls_config: ClientTlsConfig,
) -> anyhow::Result<()> {
    let address = SocketAddr::new(
        address
            .parse()
            .unwrap_or_else(|_| panic!("failed to parse address '{}'", address)),
        port,
    );

    let address = format!("https://{address}");
    let channel = Channel::from_shared(address)?
        .tls_config(tls_config)?
        .connect()
        .await?;

    info!("Using TLS over TCP socket as transport");
    execute_request(command, channel).await
}

async fn execute_request(command: &Commands, channel: Channel) -> anyhow::Result<()> {
    let mut client = BpfdClient::new(channel);
    match command {
        Commands::LoadFromFile(l) => {
            let bytecode = match command.get_bytecode_location() {
                Ok(t) => t,
                Err(e) => bail!(e),
            };

            let attach = match command.get_attach_info() {
                Ok(t) => t,
                Err(e) => bail!(e),
            };

            let request = tonic::Request::new(LoadRequest {
                bytecode,
                name: l.name.to_string(),
                program_type: l.command.get_prog_type() as u32,
                attach,
                metadata: l
                    .metadata
                    .clone()
                    .unwrap_or(vec![])
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                global_data: parse_global(&l.global),
                uuid: None,
                map_owner_id: l.map_owner_id,
            });
            let response = client.load(request).await?.into_inner();

            ProgTable::new_get_bpfd(&response.info)?.print();
            ProgTable::new_get_unsupported(&response.kernel_info)?.print();
        }

        Commands::LoadFromImage(l) => {
            let bytecode = match command.get_bytecode_location() {
                Ok(t) => t,
                Err(e) => bail!(e),
            };

            let attach = match command.get_attach_info() {
                Ok(t) => t,
                Err(e) => bail!(e),
            };

            let request = tonic::Request::new(LoadRequest {
                bytecode,
                name: l.name.to_string(),
                program_type: l.command.get_prog_type() as u32,
                attach,
                metadata: l
                    .metadata
                    .clone()
                    .unwrap_or(vec![])
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                global_data: parse_global(&l.global),
                uuid: None,
                map_owner_id: l.map_owner_id,
            });
            let response = client.load(request).await?.into_inner();

            ProgTable::new_get_bpfd(&response.info)?.print();
            ProgTable::new_get_unsupported(&response.kernel_info)?.print();
        }

        Commands::Unload(l) => {
            let request = tonic::Request::new(UnloadRequest { id: l.id });
            let _response = client.unload(request).await?.into_inner();
        }
        Commands::List(l) => {
            let prog_type_filter = l.program_type.map(|p| p as u32);

            let request = tonic::Request::new(ListRequest {
                program_type: prog_type_filter,
                // Transform metadata from a vec of tuples to an owned map.
                match_metadata: l
                    .metadata_selector
                    .clone()
                    .unwrap_or(vec![])
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                bpfd_programs_only: Some(!l.all),
            });
            let response = client.list(request).await?.into_inner();
            let mut table = ProgTable(Table::new());
            let mut prog_buckets: HashMap<String, Vec<ListResult>> = HashMap::new();

            for r in response.results {

                if r.kernel_info.is_none() {
                    bail!("All programs should have kernel information")
                }
                let kernel_info = r.kernel_info.clone().unwrap();
                
                let key = ProgramType::try_from(kernel_info.program_type)?.to_string();
                if let Some(programs) = prog_buckets.get_mut(&key) { 
                    programs.push(r);
                } else { 
                    prog_buckets.insert(key, vec![r]);
                }
            }

            for (k, v) in prog_buckets.into_iter() { 
                // Match on programs supported by bpfd
                match ProgramType::try_from(k)? {
                    ProgramType::Xdp => {
                        table.new_list_xdp();
                        for r in v.into_iter() { 
                            let info = r.kernel_info.unwrap();
                            // bpfd program
                            if let Some(p) = r.info {
                                if let Info::XdpAttachInfo(xdp_info) = p.attach.unwrap().info.unwrap() {
                                    table.add_row_list_xdp(info.id, xdp_info.priority, info.name, xdp_info.iface, xdp_info.proceed_on);
                                    continue;
                                }
                            };

                        }
                    }
                    ProgramType::Tc => {
                        let mut table = ProgTable::new_list_tc();
                        for r in v.into_iter() { 
                            let info = r.info.unwrap();
                            let attach_info = info.attach.unwrap();
                            let tc_info = match attach_info.info.unwrap() {
                                Info::TcAttachInfo(t) => t,
                                _ => bail!("Invalid attach info"),
                            };
                            table.add_row_list_tc(r.id, r.name, tc_info.iface, tc_info.direction, tc_info.proceed_on);
                        }
                        table.print();
                    }
                    ProgramType::Tracepoint => {
                        let mut table = ProgTable::new_list_tracepoint();
                        for r in v.into_iter() { 
                            let info = r.info.unwrap();
                            let tracepoint_info = match info.attach.unwrap().info.unwrap() {
                                Info::TracepointAttachInfo(t) => t,
                                _ => bail!("Invalid attach info"),
                            };
                            table.add_row_list_tracepoint(r.id, r.name, tracepoint_info.tracepoint);
                        }
                        table.print();
                    }
                    ProgramType::Probe => {
                        let mut table = ProgTable::new_list_kprobe();
                        for r in v.into_iter() { 
                            let info = r.info.unwrap();
                            let kprobe_info = match info.attach.unwrap().info
                        }
                    }
                    _ => { 

                    }
                }
            };

            table.print()
        }
        Commands::Get { id } => {
            let request = tonic::Request::new(GetRequest { id: *id });
            let response = client.get(request).await?.into_inner();

            ProgTable::new_get_bpfd(&response.info)?.print();
            ProgTable::new_get_unsupported(&response.kernel_info)?.print();
        }
        Commands::PullBytecode(l) => {
            let image: BytecodeImage = l.try_into()?;
            let request = tonic::Request::new(PullBytecodeRequest { image: Some(image) });
            let _response = client.pull_bytecode(request).await?;

            println!("Successfully downloaded bytecode");
        }
    }
    Ok(())
}
