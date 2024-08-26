use anyhow::{anyhow, Result};
use regex::Regex;
use serde::Deserialize;
use std::any::Any;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::debugger::Debugger;
use crate::metadata::*;
use crate::tools::*;

enum GdbMiRemote {
    Command(Vec<String>),
    #[allow(unused)]
    Server(String, u32),
    #[allow(unused)]
    Attach(u32),
}

impl GdbMiRemote {
    fn gdbargs(&self) -> Vec<String> {
        let mut ret = Vec::new();

        ret.push("--interpreter=mi3".to_string());

        match self {
            GdbMiRemote::Command(cmd) => {
                ret.push("--args".to_string());
                ret.append(&mut cmd.clone());
            }
            GdbMiRemote::Server(_, _) => todo!(),
            GdbMiRemote::Attach(_) => todo!(),
        }

        ret
    }
}

enum GdbMiCommandResponse {
    Done(String),
    Error(String),
    ParseError(String),
}

impl GdbMiCommandResponse {
    fn new(line: &str) -> GdbMiCommandResponse {
        if let Some(error) = line.strip_prefix("error") {
            return GdbMiCommandResponse::Error(error.to_string());
        }

        if let Some(resp) = line.strip_prefix("done") {
            return GdbMiCommandResponse::Done(resp.to_string());
        }

        if let Some(resp) = line.strip_prefix("running") {
            return GdbMiCommandResponse::Done(resp.to_string());
        }

        GdbMiCommandResponse::ParseError(line.to_string())
    }
}

impl RunState {
    fn new_from_gdb(resp: &str) -> Result<RunState> {
        if let Some(runctx) = resp.strip_prefix("running") {
            return Ok(RunState::Running(runctx.to_string()));
        }

        if let Some(stopctx) = resp.strip_prefix("stopped") {
            let stop_reason = StopReason::new(stopctx)?;
            return Ok(RunState::Stopped(Box::new(stop_reason)));
        }

        Err(anyhow!("No such gdb run state: {}", resp))
    }
}

#[derive(Deserialize, Debug)]
struct GdbSymbol {
    name: String,
    address: Option<String>,
    line: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    description: Option<String>,
}

impl GdbSymbol {
    fn to_common_symbol(&self) -> Symbol {
        Symbol {
            name: self.name.clone(),
            address: self.address.clone(),
            line: self.line.as_ref().and_then(|v| v.parse::<i32>().ok()),
            type_: self.type_.clone(),
            description: self.description.clone(),
        }
    }
}

#[derive(Deserialize, Debug)]
struct GdbFileSymbols {
    #[allow(unused)]
    filename: String,
    fullname: String,
    symbols: Vec<GdbSymbol>,
}

#[derive(Deserialize, Debug)]
struct GdbSymbolRecord {
    debug: Option<Vec<GdbFileSymbols>>,
    nondebug: Option<Vec<GdbSymbol>>,
}

pub struct GdbMiState {
    response_id: u64,
    thread_stdout: Option<JoinHandle<Result<()>>>,
    gdb_in: ChildStdin,
    running: bool,
    gdblog: Vec<String>,
    resps: HashMap<u64, GdbMiCommandResponse>,
    runstate: Option<RunState>,
}

impl GdbMiState {
    fn get_stop_state(st: &Arc<Mutex<GdbMiState>>) -> Option<StopReason> {
        if let Ok(ls) = st.lock().as_mut() {
            if let Some(RunState::Stopped(st)) = ls.runstate.clone() {
                return Some(*st);
            }
        }

        None
    }

    fn pushlog(state: Arc<Mutex<GdbMiState>>, log: &str) -> Result<()> {
        if let Ok(ls) = state.lock().as_mut() {
            ls.gdblog.push(log.to_string());
            return Ok(());
        }

        Err(anyhow!("Failed to lock"))
    }

    fn pushresp(state: Arc<Mutex<GdbMiState>>, id: u64, resp: GdbMiCommandResponse) -> Result<()> {
        if let Ok(ls) = state.lock().as_mut() {
            ls.resps.insert(id, resp);
            return Ok(());
        }

        Err(anyhow!("Failed to lock"))
    }

    fn getlog(state: Arc<Mutex<GdbMiState>>) -> Result<Vec<String>> {
        if let Ok(ls) = state.lock().as_mut() {
            let ret = ls.gdblog.clone();
            ls.gdblog.clear();
            return Ok(ret);
        }

        Err(anyhow!("Failed to lock"))
    }

    fn isrunning(state: Arc<Mutex<GdbMiState>>) -> Result<bool> {
        if let Ok(ls) = state.lock() {
            if let Some(RunState::Stopped(stop_state)) = &ls.runstate {
                /* Program did exit */
                if stop_state.exited() {
                    return Ok(false);
                }
            }

            return Ok(ls.running);
        }

        Err(anyhow!("Failed to lock"))
    }

    fn setrunstate(state: Arc<Mutex<GdbMiState>>, runstate: RunState) -> Result<()> {
        if let Ok(ls) = state.lock().as_mut() {
            ls.runstate = Some(runstate);
            return Ok(());
        }

        Err(anyhow!("Failed to lock"))
    }

    fn thread_loop<T: std::io::Read>(state: Arc<Mutex<GdbMiState>>, gdb_out: T) -> Result<()> {
        let mut output = BufReader::new(gdb_out);
        let mut line = String::new();

        while GdbMiState::isrunning(state.clone())? {
            output.read_line(&mut line)?;

            log::trace!("OUTPUT {}", line);

            if let Some(log) = line.strip_prefix("~") {
                GdbMiState::pushlog(state.clone(), log)?;
            } else if let Some((id, resp)) = parse_response_with_token("\\^", &line) {
                GdbMiState::pushresp(state.clone(), id, GdbMiCommandResponse::new(resp.as_str()))?;
            } else if let Some(srstate) = line.strip_prefix("*") {
                let rstate = RunState::new_from_gdb(srstate)?;
                GdbMiState::setrunstate(state.clone(), rstate)?;
            } else if let Some(_) = line.strip_prefix("~") {
                /* SKIPPED */
            } else if let Some(_) = line.strip_prefix("=") {
                /* SKIPPED */
            } else if let Some(_) = line.strip_prefix("&") {
                /* SKIPPED */
            } else if line.starts_with("(gdb)") {
                /* SKIPPED */
            } else if line.starts_with("\u{1b}[H") {
                /* SKIPPED : not sure what is this one ? */
            } else {
                print!("{}", line);
            }

            line.clear();
        }

        Ok(())
    }

    fn await_response(
        state: Arc<Mutex<GdbMiState>>,
        id: u64,
        timeout_ms: u128,
    ) -> Result<GdbMiCommandResponse> {
        let start_time = Instant::now();

        loop {
            if let Ok(ls) = state.lock().as_mut() {
                if let Some(resp) = ls.resps.remove(&id) {
                    return Ok(resp);
                }
            }

            if timeout_ms != 0 && start_time.elapsed().as_millis() > timeout_ms {
                return Err(anyhow!("Timeout waiting for response"));
            }
        }
    }

    fn _send_command(state: Arc<Mutex<GdbMiState>>, command: &str) -> Result<u64> {
        if let Ok(st) = state.lock().as_mut() {
            let id = st.response_id;
            st.response_id += 1;
            let cmd = format!("{}{}\n", id, command);
            st.gdb_in.write_all(cmd.as_bytes())?;
            return Ok(id);
        }

        Err(anyhow!("Failed to lock"))
    }

    fn _run_command(
        state: Arc<Mutex<GdbMiState>>,
        command: &str,
        timeout_ms: u128,
    ) -> Result<GdbMiCommandResponse> {
        let id = GdbMiState::_send_command(state.clone(), command)?;
        let resp = GdbMiState::await_response(state, id, timeout_ms)?;
        Ok(resp)
    }

    fn command(state: Arc<Mutex<GdbMiState>>, command: &str) -> Result<String> {
        match GdbMiState::_run_command(state, command, 0)? {
            GdbMiCommandResponse::Done(s) => Ok(s),
            GdbMiCommandResponse::Error(e) => Err(anyhow!("Command returned an error : {}", e)),
            GdbMiCommandResponse::ParseError(e) => Err(anyhow!("Failed to parse response : {}", e)),
        }
    }

    fn list_thread_id(state: Arc<Mutex<GdbMiState>>) -> Result<Vec<u32>> {
        let resp = GdbMiState::command(state, "-thread-list-ids")?;

        let re = Regex::new("[,\\{]thread-id=\"([0-9]+)\"")?;
        let cap: Vec<u32> = re
            .captures_iter(resp.as_str())
            .flat_map(|v| v.get(1))
            .flat_map(|v| v.as_str().parse::<u32>())
            .collect();
        Ok(cap)
    }

    fn backtrace(state: Arc<Mutex<GdbMiState>>) -> Result<Vec<DebugFrame>> {
        let resp = GdbMiState::command(state, "-stack-list-frames 0 1000")?;

        let re = Regex::new("frame=\\{([^\\}]+)\\}")?;

        let cap: Vec<DebugFrame> = re
            .captures_iter(resp.as_str())
            .flat_map(|v| v.get(1))
            .flat_map(|v| DebugFrame::new(v.as_str()))
            .collect();

        Ok(cap)
    }

    fn symbols(state: Arc<Mutex<GdbMiState>>) -> Result<SymbolTable> {
        let mut ret = SymbolTable::default();

        let resp = GdbMiState::command(state, "-symbol-info-functions --include-nondebug")?;

        if let Some(strip_start) = resp.strip_prefix(",symbols=") {
            let data = gdb_output_to_json_repr(strip_start)?;

            let symbs: GdbSymbolRecord = serde_json::from_str(&data)?;

            if let Some(per_file) = symbs.debug {
                for f in per_file {
                    ret.symbols_per_file.insert(
                        f.fullname,
                        f.symbols.iter().map(|v| v.to_common_symbol()).collect(),
                    );
                }
            }

            if let Some(nodebug) = symbs.nondebug {
                ret.symbols_per_file.insert(
                    "Unknown".to_string(),
                    nodebug.iter().map(|v| v.to_common_symbol()).collect(),
                );
            }
        }

        Ok(ret)
    }

    #[allow(unused)]
    fn locals(
        state: Arc<Mutex<GdbMiState>>,
        threadid: u32,
        frameid: u32,
    ) -> Result<Vec<(String, bool, String)>> {
        let mut ret = Vec::new();
        let cmd = format!(
            "-stack-list-variables --thread {} --frame {} --all-values",
            threadid, frameid
        );
        let resp = GdbMiState::command(state, &cmd)?;

        let groups = extract_gdb_group(&resp);

        for g in groups {
            let entries = parse_gdb_equal_list(&g);

            if let (Some(name), Some(value)) = (entries.get("name"), entries.get("value")) {
                let is_arg = if let Some(arg) = entries.get("arg") {
                    arg == "1"
                } else {
                    false
                };

                ret.push((name.to_string(), is_arg, value.to_string()));
            }
        }

        Ok(ret)
    }

    fn snapshot(state: Arc<Mutex<GdbMiState>>) -> Result<ProgramSnapshot> {
        let mut ret: HashMap<u32, Vec<DebugFrame>> = HashMap::new();

        let threads = GdbMiState::list_thread_id(state.clone())?;

        for th in threads {
            GdbMiState::select_thread(state.clone(), th)?;
            let bt = GdbMiState::backtrace(state.clone())?;

            //for frame in bt.iter_mut() {
            //    if let Ok(vars) = GdbMiState::locals(state.clone(), th, frame.level) {
            //        frame.attach_locals(vars);
            //    }
            //}

            ret.insert(th, bt);
        }

        let stop_state: Option<StopReason> = GdbMiState::get_stop_state(&state);

        Ok(ProgramSnapshot {
            state: ret,
            stop_state,
        })
    }

    fn select_thread(state: Arc<Mutex<GdbMiState>>, id: u32) -> Result<()> {
        let cmd = format!("-thread-select {}", id);
        GdbMiState::command(state, cmd.as_str())?;
        Ok(())
    }

    fn start(state: Arc<Mutex<GdbMiState>>, gdb_out: ChildStdout) -> Result<()> {
        if let Ok(st) = state.lock().as_mut() {
            let pstate = state.clone();
            let thout = std::thread::spawn(move || GdbMiState::thread_loop(pstate, gdb_out));
            st.thread_stdout = Some(thout);
        }

        Ok(())
    }

    fn new(
        gdb_in: Option<ChildStdin>,
        gdb_out: Option<ChildStdout>,
    ) -> Result<Arc<Mutex<GdbMiState>>> {
        if let (Some(gdb_in), Some(gdb_out)) = (gdb_in, gdb_out) {
            let ret = GdbMiState {
                response_id: 0,
                thread_stdout: None,
                gdb_in,
                running: true,
                gdblog: Vec::new(),
                resps: HashMap::new(),
                runstate: None,
            };

            let ret = Arc::new(Mutex::new(ret));

            GdbMiState::start(ret.clone(), gdb_out)?;

            Ok(ret)
        } else {
            Err(anyhow!("Failed to capture GDB child process stdin/stdout"))
        }
    }
}

pub struct GdbMi {
    id: u64,
    target: GdbMiRemote,
    state: Option<Arc<Mutex<GdbMiState>>>,
    child_proc: Option<Child>,
}

impl Debugger for GdbMi {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn set_id(&mut self, id: u64) {
        self.id = id;
    }

    fn get_id(&self) -> u64 {
        self.id
    }

    /// Start the debugged program (program is not started by default)
    fn start(&mut self) -> Result<()> {
        self.cmd("-exec-run")?;
        Ok(())
    }

    /// If the process is running send a stop signal to interupt it
    fn stop(&mut self) -> Result<()> {
        if self.id_is_stopped(self.id)? {
            /* Already stopped */
            return Ok(());
        }
        self.cmd("-exec-interrupt --all")?;
        Ok(())
    }

    /// Continue an interrupted process
    fn cont(&mut self) -> Result<()> {
        if self.id_is_running(self.id)? {
            /* Already running */
            return Ok(());
        }
        self.cmd("-exec-continue")?;
        Ok(())
    }

    /// Get current state of the debugged process
    fn state(&mut self) -> Result<HashMap<u64, RunState>> {
        let mut ret = HashMap::new();

        if let Some(st) = &self.state {
            if let Ok(ls) = st.lock() {
                if let Some(rs) = &ls.runstate {
                    ret.insert(self.get_id(), rs.clone());
                }
            }
        }

        Ok(ret)
    }

    /// This generate a complete state snapshot of the program
    /// You need to have the program in a stopped state first
    ///     - Calling `stop` to interrupt
    ///     - Checking `is_stopped` to handle breakpoints or crashes
    fn snapshot(&mut self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>> {
        if self.id_is_running(self.id)? {
            self.stop()?;
        }

        let exited = self.id_is_exited(self.id)?;

        if let Some(st) = &self.state {
            if exited {
                let stop_state: Option<StopReason> = GdbMiState::get_stop_state(st);
                let mut ret = HashMap::new();

                ret.insert(self.id, ProgramSnapshot::exited(stop_state));

                return Ok(ProgramSnapshot::generate_components(ret));
            }

            let snap = GdbMiState::snapshot(st.clone())?;

            let mut ret = HashMap::new();
            ret.insert(self.id, snap);
            /* Map to snapshot */
            let ret = ProgramSnapshot::generate_components(ret);
            return Ok(ret);
        }

        Err(anyhow!("Program is not running"))
    }

    /// Get the symbol table from the target split it per file
    fn symbols(&mut self) -> Result<SymbolTable> {
        if self.id_is_running(self.id)? {
            return Err(anyhow!("Symbols can only be retrieved on a stopped target"));
        }

        if let Some(st) = &self.state {
            return GdbMiState::symbols(st.clone());
        }

        Err(anyhow!("No GDB state was available to retrieve symbols"))
    }

    fn count(&mut self) -> Result<u64> {
        Ok(1)
    }
}

impl GdbMi {
    fn _start_gdb(&mut self) -> Result<()> {
        let gdbargs = self.target.gdbargs();

        log::debug!("{:?}", gdbargs);

        let mut command = Command::new("gdb")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .args(&gdbargs)
            .spawn()?;

        let state = GdbMiState::new(command.stdin.take(), command.stdout.take())?;

        self.child_proc = Some(command);
        self.state = Some(state);

        Ok(())
    }

    pub fn take_child(&mut self) -> Option<Child> {
        self.child_proc.take()
    }

    /// Run an arbitraty GDB-MI command on the target
    pub fn cmd(&mut self, command: &str) -> Result<String> {
        if let Some(st) = &self.state {
            let ret = GdbMiState::command(st.clone(), command)?;
            return Ok(ret);
        }

        Err(anyhow!("Program is not running"))
    }

    /// Gets the log output from GDB (can be safely ignored)
    /// The log is drained each time this is called.
    pub fn log(&self) -> Option<Vec<String>> {
        if let Some(st) = &self.state {
            if let Ok(r) = GdbMiState::getlog(st.clone()) {
                if !r.is_empty() {
                    return Some(r);
                } else {
                    return None;
                }
            }
        }

        None
    }

    /// Launch a command wrapped in GDB
    ///
    /// Note this does not start the underlying program you need to call `start` to do so
    pub fn run(cmd: &[&str]) -> Result<GdbMi> {
        let cmd: Vec<String> = cmd.iter().map(|v| v.to_string()).collect();
        let mut ret = GdbMi {
            target: GdbMiRemote::Command(cmd),
            state: None,
            id: 0,
            child_proc: None,
        };

        ret._start_gdb()?;

        ret.cmd("-gdb-set mi-async on")?;
        ret.cmd("-enable-pretty-printing")?;

        Ok(ret)
    }

    /**
       pub fn server(host: String, port: u32) -> Result<GdbMi> {
           unimplemented!("No server support yet");

           let ret = GdbMi {
               target: GdbMiRemote::Server(host, port),
               state: None,
               id: 0,
           };

           Ok(ret)
       }

       pub fn attach(pid: u32) -> Result<GdbMi> {
           unimplemented!("No attach support yet");

           let ret = GdbMi {
               target: GdbMiRemote::Attach(pid),
               state: None,
               id: 0,
           };

           todo!("Not done yet");

           Ok(ret)
       }
    */

    pub fn instance(self) -> Arc<Mutex<Box<dyn Debugger>>> {
        let dbg: Arc<Mutex<Box<dyn Debugger>>> = Arc::new(Mutex::new(Box::new(self)));
        dbg
    }
}
