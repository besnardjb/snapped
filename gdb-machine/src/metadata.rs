use anyhow::Result;
use gethostname::gethostname;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
};

use crate::tools::{dominating_numa_id, parse_gdb_equal_list};

#[derive(Serialize, Deserialize, Debug)]
pub struct TreeIdFactory {
    root_id: u64,
    dynamic: u64,
    stride: u64,
    offset: u64,
}

const TREE_ARITY: u64 = 24;

impl TreeIdFactory {
    pub fn default() -> TreeIdFactory {
        TreeIdFactory {
            root_id: 0,
            dynamic: std::u64::MAX,
            stride: (std::u64::MAX - 1) / TREE_ARITY,
            offset: 0,
        }
    }

    pub fn inherit(&mut self) -> Result<TreeIdFactory> {
        let root_id = self.root_id + 1 + self.stride * self.offset;
        self.offset += 1;

        let dynamic = (self.dynamic - 1) / TREE_ARITY;

        let stride = dynamic / TREE_ARITY;

        Ok(TreeIdFactory {
            root_id,
            dynamic,
            stride,
            offset: 0,
        })
    }

    pub fn id(&self) -> u64 {
        self.root_id
    }

    pub fn full(&self) -> bool {
        self.offset == TREE_ARITY
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessInfo {
    pub mpirank: Option<u32>,
    pub pid: u64,
    pub hostname: String,
    pub locality_descriptor: String,
}

impl ProcessInfo {
    fn locality_descriptor() -> Result<String> {
        let host = String::from(gethostname().as_os_str().to_str().unwrap());
        let numa = dominating_numa_id().unwrap_or(0);
        let pid = std::process::id();

        Ok(format!("{}-{}-{}", host, numa, pid))
    }

    pub fn default() -> Result<ProcessInfo> {
        let locality_descriptor = ProcessInfo::locality_descriptor()?;
        let hostname = String::from(gethostname().as_os_str().to_str().unwrap());
        let pid = std::process::id() as u64;
        let mpirank = std::env::var("PMI_RANK")
            .ok()
            .and_then(|v| v.parse::<u32>().ok());

        Ok(ProcessInfo {
            mpirank,
            pid,
            hostname,
            locality_descriptor,
        })
    }
}

#[derive(Hash, Serialize, Deserialize, Debug, Clone)]
pub struct DisplayState {
    pub reason: String,
    pub signal_name: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Hash, Serialize, Deserialize, Debug, Clone)]
pub struct DisplayFrame {
    pub func: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Hash, Serialize, Deserialize, Debug, Clone)]
pub enum BacktraceState {
    Frame(DisplayFrame),
    State(DisplayState),
}

impl From<&DebugFrame> for BacktraceState {
    fn from(value: &DebugFrame) -> Self {
        BacktraceState::Frame(DisplayFrame {
            func: value.func.clone(),
            file: value.fullname.clone(),
            line: value.line.clone(),
        })
    }
}

impl From<&StopReason> for BacktraceState {
    fn from(value: &StopReason) -> Self {
        BacktraceState::State(DisplayState {
            reason: value.reason.clone(),
            signal_name: value.signal_name.clone(),
            exit_code: value.exit_code.clone(),
        })
    }
}

impl BacktraceState {
    pub fn root() -> BacktraceState {
        BacktraceState::Frame(DisplayFrame {
            func: ".".to_string(),
            file: None,
            line: None,
        })
    }

    fn print(&self) -> String {
        match &self {
            BacktraceState::Frame(b) => format!(
                "{} {}:{}",
                b.func,
                b.file.clone().unwrap_or("Unknown".to_string()),
                b.line.unwrap_or(0),
            ),
            BacktraceState::State(s) => {
                format!(
                    "{} {}",
                    s.reason,
                    s.signal_name.clone().unwrap_or("".to_string()),
                )
            }
        }
    }

    pub fn get_hash(&self) -> u64 {
        let mut hash = DefaultHasher::new();
        self.hash(&mut hash);
        hash.finish()
    }
}

/// Represents a stack frame
///
/// This struct contains metadata about a function call or execution point in a program,
/// including its level, address, function name, file and line number information, and any
/// additional arguments or local variables.
#[derive(Debug, Serialize, Deserialize)]
pub struct DebugFrame {
    /// The level of this debug frame (e.g. 0 for the current frame)
    pub level: u32,

    /// The memory address of this debug frame
    pub addr: String,

    /// The name of the function that is currently being executed or called
    pub func: String,

    /// The file and line number information for this debug frame (if available)
    pub file: Option<String>,

    /// The absolute path to the file
    pub fullname: Option<String>,

    /// The line in the given file
    pub line: Option<u32>,

    /// Information about the origin of this debug frame (e.g. where it was called from)
    pub from: Option<String>,

    /// The architecture or platform that this debug frame is relevant to
    pub arch: Option<String>,

    /// Additional arguments passed to the function or execution point represented by this frame
    pub args: Option<Vec<(String, String)>>,

    /// Local variables and their values at this execution point
    pub locals: Option<Vec<(String, String)>>,
}

impl DebugFrame {
    pub fn exited() -> DebugFrame {
        DebugFrame {
            level: 0,
            addr: "".to_string(),
            func: "Process has exited (no stack)".to_string(),
            file: None,
            fullname: None,
            line: None,
            from: None,
            arch: None,
            args: None,
            locals: None,
        }
    }

    /// Creates a new `DebugFrame` from a GDB-MI backtrace state.
    ///
    /// This function parses the given string, which represents a frame in a GDB-MI backtrace
    /// state, and returns a `Result` containing the parsed `DebugFrame`. If parsing fails,
    /// the returned `Result` will be an error.
    pub fn new(desc: &str) -> Result<DebugFrame> {
        let entries = parse_gdb_equal_list(desc);

        let mut ret = DebugFrame {
            level: 0,
            addr: "".to_string(),
            func: "".to_string(),
            file: None,
            fullname: None,
            line: None,
            from: None,
            arch: None,
            args: None,
            locals: None,
        };

        if let Some(level) = entries.get("level") {
            ret.level = level.parse::<u32>().unwrap_or(0);
        }

        if let Some(addr) = entries.get("addr") {
            ret.addr = addr.clone();
        }

        if let Some(func) = entries.get("func") {
            ret.func = func.clone();
        }

        if let Some(file) = entries.get("file") {
            ret.file = Some(file.clone());
        }

        if let Some(fullname) = entries.get("fullname") {
            ret.fullname = Some(fullname.clone());
        }

        if let Some(line) = entries.get("line") {
            ret.line = line.parse::<u32>().ok();
        }

        if let Some(from) = entries.get("from") {
            ret.from = Some(from.clone());
        }

        if let Some(arch) = entries.get("arch") {
            ret.arch = Some(arch.clone());
        }

        Ok(ret)
    }

    /// Attaches additional arguments and local variables from a variable list.
    ///
    /// The `vars` parameter should be in the shape `(name, is_argument, value)`, where `is_argument`
    /// determines whether the variable represents an argument or a local variable. If the variable is
    /// an argument, it will be added to the `args` field; otherwise, it will be added to the `locals` field.
    pub fn attach_locals(&mut self, vars: Vec<(String, bool, String)>) {
        let mut args: Vec<(String, String)> = Vec::new();
        let mut locals: Vec<(String, String)> = Vec::new();

        for (name, is_arg, value) in vars {
            if is_arg {
                args.push((name, value));
            } else {
                locals.push((name, value));
            }
        }

        if !args.is_empty() {
            self.args = Some(args);
        }

        if !locals.is_empty() {
            self.locals = Some(locals);
        }
    }

    /// Serialize a `DebugFrame` to a JSON string
    pub fn json(&self) -> Result<String> {
        let ret = serde_json::to_string(&self)?;
        Ok(ret)
    }

    fn descriptor(&self) -> BacktraceState {
        BacktraceState::from(self)
    }

    fn to_component(comp: &Vec<DebugFrame>) -> Vec<BacktraceState> {
        comp.iter().map(|v| v.descriptor()).collect()
    }

    fn hash_component(comp: &Vec<BacktraceState>) -> u64 {
        let mut hash = DefaultHasher::new();
        comp.hash(&mut hash);
        hash.finish()
    }

    pub fn pretty_print_component(mut comp: Vec<(u64, Vec<BacktraceState>)>) {
        comp.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        println!("=============");

        for (cnt, btc) in comp.iter().enumerate() {
            println!("Stack #{} with {} contributors:", cnt, btc.0);
            for s in &btc.1 {
                println!("\t{}", s.print());
            }
            println!("=============");
        }
    }
}

/// Represents the full state of a program, including the list of stack frames for each individual thread.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProgramSnapshot {
    /// A map where the keys are thread IDs and the values are lists of `DebugFrame`s representing that thread's call stack.
    pub state: HashMap<u32, Vec<DebugFrame>>,
    pub stop_state: Option<StopReason>,
}

impl ProgramSnapshot {
    pub fn exited(stop_state: Option<StopReason>) -> ProgramSnapshot {
        let mut state = HashMap::new();
        state.insert(0, vec![DebugFrame::exited()]);

        ProgramSnapshot { state, stop_state }
    }

    pub fn json(&self) -> Result<String> {
        let ret = serde_json::to_string_pretty(&self)?;
        Ok(ret)
    }

    pub fn generate_components(
        dist_state: HashMap<u64, ProgramSnapshot>,
    ) -> HashMap<u64, (u64, Vec<BacktraceState>)> {
        let mut components: HashMap<u64, (u64, Vec<BacktraceState>)> = HashMap::new();

        for snap in dist_state.values() {
            for thsnap in snap.state.values() {
                let mut comp = if let Some(stop_reason) = &snap.stop_state {
                    //println!("{:?}", stop_reason);

                    /* If we add SIGINT it will fill the whole stack in
                    case of manual interrupt  */
                    match stop_reason.is_sigint() {
                        true => Vec::new(),
                        false => vec![BacktraceState::from(stop_reason)],
                    }
                } else {
                    Vec::new()
                };

                comp.append(&mut DebugFrame::to_component(thsnap));

                let hash = DebugFrame::hash_component(&comp);

                if let Some((cnt, _)) = components.get_mut(&hash) {
                    *cnt += 1;
                } else {
                    components.insert(hash, (1, comp));
                }
            }
        }

        components
    }

    pub fn components_vec(
        components: &HashMap<u64, (u64, Vec<DisplayFrame>)>,
    ) -> Vec<(u64, Vec<DisplayFrame>)> {
        components.iter().map(|(_, v)| v).cloned().collect()
    }

    pub fn components_merge(
        mut components: Vec<HashMap<u64, (u64, Vec<BacktraceState>)>>,
    ) -> HashMap<u64, (u64, Vec<BacktraceState>)> {
        if let Some(mut first) = components.pop() {
            for maps in components {
                for (hash, (cnt, vec)) in maps {
                    if let Some((targ_cnt, _)) = first.get_mut(&hash) {
                        *targ_cnt += cnt;
                    } else {
                        first.insert(hash, (cnt, vec));
                    }
                }
            }

            return first;
        }

        HashMap::new()
    }
}

/// This describes the stop state of a program
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopReason {
    /// The reason for the stop (e.g. "exited normally", "signal received", etc.)
    pub reason: String,

    /// A brief description of the stop reason
    pub disp: Option<String>,

    /// The number of the breakpoint that caused the stop, if applicable
    pub breakpoint_num: Option<u32>,

    /// The address at which the program stopped executing
    pub addr: Option<String>,

    /// The function in which the program stopped executing
    pub function: Option<String>,

    /// A brief description of what the `function` is (e.g. "main", "func", etc.)
    pub meaning: Option<String>,

    /// Name of the stopping signal
    #[serde(rename = "signal_name")]
    pub signal_name: Option<String>,

    /// The file in which the program stopped executing
    pub file: Option<String>,

    /// The full path to the file where the program stopped executing
    pub fullname: Option<String>,

    /// The line number at which the program stopped executing
    pub line: Option<u32>,

    /// The architecture type of the target being debugged (e.g. "i386", "x86_64", etc.)
    pub arch: Option<String>,

    /// The ID of the thread that caused the stop, if applicable
    pub thread_id: Option<u32>,

    /// A comma-separated list of IDs for the threads that stopped
    pub stopped_threads: Option<String>,

    /// The core ID of the CPU where the program stopped executing (if multi-core)
    pub core: Option<u32>,

    /// The exit code returned by the program, if it exited normally
    pub exit_code: Option<i32>,
}

impl StopReason {
    pub fn is_sigint(&self) -> bool {
        if let Some(sig) = &self.signal_name {
            if sig == "SIGINT" {
                return true;
            }
        }

        false
    }

    pub fn new(resp: &str) -> Result<StopReason> {
        let arg_re = Regex::new("(args=\\[.*\\])")?;
        let resp_no_arg = arg_re.replace(resp, "");

        let map = parse_gdb_equal_list(&resp_no_arg);

        let stop_reason = StopReason {
            reason: map.get("reason").cloned().unwrap_or_default(),
            disp: map.get("disp").cloned().map(|s| s.to_string()),
            breakpoint_num: map
                .get("breakpoint_num")
                .and_then(|s| s.parse::<u32>().ok()),
            addr: map.get("addr").cloned(),
            function: map.get("function").cloned(),
            meaning: map.get("meaning").cloned(),
            signal_name: map.get("signal-name").cloned(),
            file: map.get("file").cloned(),
            fullname: map.get("fullname").cloned(),
            line: map.get("line").and_then(|s| s.parse::<u32>().ok()),
            arch: map.get("arch").cloned(),
            thread_id: map.get("thread_id").and_then(|s| s.parse::<u32>().ok()),
            stopped_threads: map.get("stopped_threads").cloned(),
            core: map.get("core").and_then(|s| s.parse::<u32>().ok()),
            exit_code: map.get("exit-code").and_then(|s| s.parse::<i32>().ok()),
        };

        Ok(stop_reason)
    }

    pub fn exited(&self) -> bool {
        self.reason == "exited" || self.reason == "exited-normally"
    }
}

/// Describes the state of a program being debugged
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RunState {
    Stopped(Box<StopReason>),
    Running(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Symbol {
    /// The name of the symbol
    pub name: String,
    /// The address associated with this symbol (if applicable)
    pub address: Option<String>,
    /// The line number associated with this symbol (if applicable)
    pub line: Option<i32>,
    #[serde(rename = "type")]
    /// The typemap of symbol
    pub type_: Option<String>,
    /// The declaration of the Symbol
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SymbolTable {
    /// Mapping of file names to a list of symbols in that file
    pub symbols_per_file: HashMap<String, Vec<Symbol>>,
}

impl SymbolTable {
    pub fn default() -> SymbolTable {
        SymbolTable {
            symbols_per_file: HashMap::new(),
        }
    }
}
