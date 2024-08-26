//! Wrapper around the GDB-MI interface with server and reduction-tree support
//!
//! This crate implements a wrapping around the GDB-MI inteface with spatial
//! aggregation using a tree-based overlay network (TBON)
//!
//! See [crate::debugger::Debugger] for the Debugger interface.
//!
//! This interface is shared by multiple entrypoints instanciated with the following :
//! - Single debugger instance:
//!     - [crate::GdbMachine::local] a local debugger
//! - TBON instance distributed debugging:
//!     - [crate::GdbMachine::run_as_root] a tree root debugger (no local debugger)
//!     - [crate::GdbMachine::run_as_leaf] a distributed debugger connecting to a r
//! - Client-server model:
//!     - [crate::GdbClient::new] a client connecting to a running server
//!     - [crate::GdbMachine::new] and [crate::GdbMachine::url] start a server and get its address
//!
//! # Example Local DBG
//!
//! ```
//! // Start a local debugger instance
//! let mut dbg = GdbMachine::local(cmd)?;
//! // Run the debuggee
//! dbg.start()?
//!
//! // Wait for a process to crash
//! loop {
//!   if !dbg.all_running()? {
//!     /* Some process stopped */
//!     /* Stop all  */
//!     dbg.stop()?;
//!      break;
//!    }
//!  }
//!  /* Get processes's state */
//!  let snap = dbg.snapshot()?;
//! ```
//!

pub mod debugger;
pub mod gdbmi;
pub mod metadata;
mod protocol;
mod tools;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use debugger::Debugger;
use debugger::DummyDebugger;
use gdbmi::GdbMi;
use metadata::BacktraceState;
use metadata::ProcessInfo;
use metadata::ProgramSnapshot;
use metadata::RunState;
use metadata::SymbolTable;
use metadata::TreeIdFactory;
use protocol::GdbMachineResponse;
use rayon::prelude::*;
use rayon::scope;
use std::any::Any;
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::process::Child;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use std::u64;
use tools::read_until_null;
use tools::strdistance;

use crate::protocol::GdbMachineCommand;

pub struct GdbClient {
    client_sock: TcpStream,
}

impl GdbClient {
    pub fn new(addr: &str) -> Result<GdbClient> {
        let client_sock = TcpStream::connect(addr)?;

        Ok(GdbClient { client_sock })
    }

    fn do_command(&mut self, cmd: &GdbMachineCommand) -> Result<GdbMachineResponse> {
        let cmd_in_json = serde_json::to_string(&cmd)?;

        /* Write JSON */
        self.client_sock.write_all(cmd_in_json.as_bytes())?;
        /* Write Separator */
        self.client_sock.write_all("\0".as_bytes())?;
        self.client_sock.flush()?;

        /* Get Response */
        let resp = read_until_null(&mut self.client_sock)?;
        /* Parse Response */
        let ret: GdbMachineResponse = serde_json::from_str(&resp)?;

        Ok(ret)
    }

    pub fn join(&mut self, targ: String) -> Result<()> {
        self.do_command(&GdbMachineCommand::Join(targ))?.ok()
    }

    pub fn pivot(&mut self, local_url: String) -> Result<(u64, String)> {
        let process_info = ProcessInfo::default()?;

        let ret = self.do_command(&GdbMachineCommand::Pivot(process_info, local_url))?;

        if let GdbMachineResponse::Pivot(id, targ) = ret {
            return Ok((id, targ));
        }

        Err(anyhow!("Bad response for pivot"))
    }
}

impl Debugger for GdbClient {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    #[allow(unused)]
    fn set_id(&mut self, id: u64) {}

    fn get_id(&self) -> u64 {
        0
    }

    /// Start the debugged program
    fn start(&mut self) -> Result<()> {
        self.do_command(&GdbMachineCommand::Start)?.ok()
    }
    /// Stop the debugged program
    fn stop(&mut self) -> Result<()> {
        self.do_command(&GdbMachineCommand::Stop)?.ok()
    }
    /// Continue a stopped program
    fn cont(&mut self) -> Result<()> {
        self.do_command(&GdbMachineCommand::Continue)?.ok()
    }

    /// Get current state of program
    fn state(&mut self) -> Result<HashMap<u64, RunState>> {
        let st = self.do_command(&GdbMachineCommand::GetState)?.state();

        Ok(st)
    }

    /// Snapshot a stopped program
    fn snapshot(&mut self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>> {
        self.do_command(&GdbMachineCommand::GetSnapshot)?.snapshot()
    }

    /// Get Symbol table
    fn symbols(&mut self) -> Result<SymbolTable> {
        self.do_command(&GdbMachineCommand::GetSymbols)?.symbols()
    }

    fn count(&mut self) -> Result<u64> {
        Ok(1)
    }
}

pub struct TreeState {
    id: Option<u64>,
    seen_children: HashMap<String, (String, TreeIdFactory)>,
    children: Vec<GdbClient>,
}

impl TreeState {
    fn default() -> TreeState {
        TreeState {
            seen_children: HashMap::new(),
            children: Vec::new(),
            id: None,
        }
    }

    fn set_root(&mut self, root_url: String) {
        self.seen_children
            .insert("ROOT".to_string(), (root_url, TreeIdFactory::default()));
    }

    fn _pivot_get_closest_id_match(&mut self, locator: &String) -> Result<(String, TreeIdFactory)> {
        /* Exact match */
        if let Some(_) = self.seen_children.get(locator) {
            return Err(anyhow!("Process {:?} is already registered", locator));
        }

        /* If root is not full use it */
        let (url, root) = self
            .seen_children
            .get_mut("ROOT")
            .expect("It is only possible to pivot on root process");

        if !root.full() {
            log::info!("PIVOT {} is Joining ROOT", locator);
            return Ok((url.clone(), root.inherit()?));
        }

        /* Now attempt closest match */
        let mut mtch = None;
        let mut distance: u64 = u64::MAX;

        self.seen_children
            .iter()
            .filter(|(_, v)| !v.1.full())
            .for_each(|(k, _)| {
                let dist = strdistance(k, &locator);

                if dist < distance {
                    distance = dist;
                    mtch = Some(k.clone());
                }
            });

        match mtch {
            Some(m) => {
                if m == "ROOT" {
                    unimplemented!("Distance match should not capture root");
                }
                /* We are sure the key exists as k is from keys */
                if let Some((url, val)) = self.seen_children.get_mut(&m) {
                    log::info!("PIVOT {} is Joining {}", locator, m);
                    return Ok((url.clone(), val.inherit()?));
                } else {
                    unreachable!("Key must exist");
                }
            }
            None => unreachable!("Root should have had at least another child"),
        }
    }

    fn pivot(&mut self, process_info: &ProcessInfo, from: String) -> Result<(u64, String)> {
        /* Generate range for new entry */
        let (url, new_range) =
            self._pivot_get_closest_id_match(&process_info.locality_descriptor)?;

        /* Let new id */
        let id = new_range.id();

        /* Insert range to locator */
        self.seen_children
            .insert(process_info.locality_descriptor.clone(), (from, new_range));

        Ok((id, url))
    }

    fn join(&mut self, targ: String) -> Result<()> {
        let client = GdbClient::new(targ.as_str())?;
        self.children.push(client);
        Ok(())
    }

    fn run_on_children(&mut self, cmd: GdbMachineCommand) -> Result<Vec<GdbMachineResponse>> {
        let ret = self
            .children
            .par_iter_mut()
            .map(|c| c.do_command(&cmd))
            .collect::<Vec<_>>();

        let ret: Result<Vec<GdbMachineResponse>> = ret.into_iter().collect();

        ret
    }

    fn all_resp_ok(resps: &Vec<GdbMachineResponse>) -> Result<()> {
        let errs: Vec<String> = resps
            .iter()
            .filter_map(|v| match v {
                GdbMachineResponse::Error(e) => Some(e.to_string()),
                _ => None,
            })
            .collect();

        if !errs.is_empty() {
            return Err(anyhow!("{}", errs.join(",")));
        }

        Ok(())
    }

    /**
       fn check_is_root(&mut self) -> Result<&mut TreeState> {
           if let Some(id) = self.id {
               if id != 1 {
                   return Err(anyhow!("Commands can only be run on a root GdbMachine"));
               }
           } else {
               return Err(anyhow!("Commands can only be run on a root GdbMachine"));
           }

           Ok(self)
       }
    */

    fn merge_results(
        res1: GdbMachineResponse,
        res2: Option<GdbMachineResponse>,
    ) -> GdbMachineResponse {
        let mut resp: Option<GdbMachineResponse> = None;

        if res2.is_none() {
            return res1;
        }

        if let (r1, Some(r2)) = (res1, res2) {
            resp = match r1 {
                GdbMachineResponse::Error(e) => Some(GdbMachineResponse::Error(e.to_string())),
                GdbMachineResponse::Ok => match r2 {
                    GdbMachineResponse::Error(e) => Some(GdbMachineResponse::Error(e.to_string())),
                    GdbMachineResponse::Ok => Some(GdbMachineResponse::Ok),
                    _ => Some(GdbMachineResponse::Error(
                        "Incompatible type to be merged Ok".to_string(),
                    )),
                },
                GdbMachineResponse::State(mut st1) => match r2 {
                    GdbMachineResponse::State(st2) => {
                        st1.extend(st2.into_iter());
                        Some(GdbMachineResponse::State(st1))
                    }
                    GdbMachineResponse::Error(e) => Some(GdbMachineResponse::Error(e.to_string())),
                    _ => Some(GdbMachineResponse::Error(
                        "Incompatible type to be merged State".to_string(),
                    )),
                },
                GdbMachineResponse::Snapshot(st1) => match r2 {
                    GdbMachineResponse::Snapshot(st2) => Some(GdbMachineResponse::Snapshot(
                        ProgramSnapshot::components_merge(vec![st1, st2]),
                    )),
                    GdbMachineResponse::Error(e) => Some(GdbMachineResponse::Error(e.to_string())),
                    _ => Some(GdbMachineResponse::Error(
                        "Incompatible type to be merged snapshot".to_string(),
                    )),
                },
                GdbMachineResponse::Count(c1) => match r2 {
                    GdbMachineResponse::Count(c2) => Some(GdbMachineResponse::Count(c1 + c2)),
                    GdbMachineResponse::Error(e) => Some(GdbMachineResponse::Error(e.to_string())),

                    _ => Some(GdbMachineResponse::Error(
                        "Incompatible type to be merged Count".to_string(),
                    )),
                },
                GdbMachineResponse::Symbols(_) => todo!(),
                GdbMachineResponse::Pivot(_, _) => {
                    todo!()
                }
            }
        }

        if let Some(resp) = resp {
            return resp;
        }

        GdbMachineResponse::Error(format!("Conversion case not handled"))
    }
}

impl Debugger for TreeState {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn set_id(&mut self, id: u64) {
        self.id = Some(id);
    }

    fn get_id(&self) -> u64 {
        if let Some(id) = self.id {
            return id;
        }

        0
    }

    fn start(&mut self) -> Result<()> {
        if self.children.is_empty() {
            return Ok(());
        }

        TreeState::all_resp_ok(&self.run_on_children(GdbMachineCommand::Start)?)
    }

    fn count(&mut self) -> Result<u64> {
        if self.children.is_empty() {
            return Ok(0);
        }

        let ret = &self.run_on_children(GdbMachineCommand::Count)?;

        let ret = ret
            .iter()
            .filter_map(|v| {
                if let GdbMachineResponse::Count(c) = v {
                    Some(c)
                } else {
                    None
                }
            })
            .sum::<u64>();

        Ok(ret)
    }

    fn stop(&mut self) -> Result<()> {
        if self.children.is_empty() {
            return Ok(());
        }

        TreeState::all_resp_ok(&self.run_on_children(GdbMachineCommand::Stop)?)
    }

    fn cont(&mut self) -> Result<()> {
        if self.children.is_empty() {
            return Ok(());
        }

        TreeState::all_resp_ok(&self.run_on_children(GdbMachineCommand::Continue)?)
    }

    fn state(&mut self) -> Result<HashMap<u64, RunState>> {
        let mut ret = HashMap::new();

        if self.children.is_empty() {
            return Ok(ret);
        }

        let resps = self.run_on_children(GdbMachineCommand::GetState)?;

        TreeState::all_resp_ok(&resps)?;

        for resp in resps {
            if let GdbMachineResponse::State(st) = resp {
                ret.extend(st.into_iter());
            }
        }

        Ok(ret)
    }

    fn snapshot(&mut self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>> {
        if self.children.is_empty() {
            return Ok(HashMap::new());
        }

        let resps = self.run_on_children(GdbMachineCommand::GetSnapshot)?;

        TreeState::all_resp_ok(&resps)?;

        let components: Vec<HashMap<u64, (u64, Vec<BacktraceState>)>> = resps
            .into_iter()
            .filter_map(|v| {
                if let GdbMachineResponse::Snapshot(st) = v {
                    return Some(st);
                }
                None
            })
            .collect();

        Ok(ProgramSnapshot::components_merge(components))
    }

    fn symbols(&mut self) -> Result<SymbolTable> {
        todo!()
    }
}

pub struct GdbMachine {
    listening_sock: TcpListener,
    host: String,
    dbg: Arc<Mutex<Box<dyn Debugger>>>,
    state: Arc<Mutex<Box<dyn Debugger>>>,
}

impl GdbMachine {
    pub fn new(bindaddr: &str, dbg: Arc<Mutex<Box<dyn Debugger>>>) -> Result<GdbMachine> {
        let address = SocketAddr::from_str(bindaddr)?;

        let listening_sock = TcpListener::bind(address)?;

        let host = gethostname::gethostname()
            .to_str()
            .context("Failed to convert hostname to string")?
            .to_string();

        let ret = GdbMachine {
            listening_sock,
            host,
            dbg,
            state: Arc::new(Mutex::new(Box::new(TreeState::default()))),
        };

        Ok(ret)
    }

    pub fn local(command: &[String]) -> Result<RootDebugger> {
        let v: Vec<&str> = command.iter().map(|x| &**x).collect();
        let mut gdb = GdbMi::run(v.as_slice())?;

        let child_proc = gdb.take_child();

        return Ok(RootDebugger {
            state: Arc::new(Mutex::new(Box::new(gdb))),
            child_proc,
        });
    }

    pub fn run_as_leaf(root: String, command: &[String]) -> Result<()> {
        let v: Vec<&str> = command.iter().map(|x| &**x).collect();
        let gdb = GdbMi::run(v.as_slice())?;

        let server = GdbMachine::new("0.0.0.0:0", gdb.instance())?;

        let mut client = GdbClient::new(&root)?;

        let (id, targ) = client.pivot(server.url()?)?;

        server.set_id(id);

        // At this point the server should be backconnected
        // We can drop our current client to the root
        drop(client);

        // Now we notify the new client we want him to join us
        let mut client = GdbClient::new(&targ)?;
        client.join(server.url()?)?;
        //We are done the targ is conncted to our local server
        drop(client);

        server.run()?;

        Ok(())
    }

    pub fn wait_for_child(&self, child_count: usize) -> Result<()> {
        /* Wait for all clients to join in */
        loop {
            if let Some(cnt) = self.tree_count() {
                if cnt == child_count {
                    let tree_count = self.state.lock().unwrap().count()?;
                    log::trace!("Tree is currently hosting {} processes", tree_count);
                    if cnt == tree_count as usize {
                        break;
                    }
                }
            }
            sleep(Duration::from_millis(500));
        }

        Ok(())
    }

    pub fn run_as_root() -> Result<(Arc<GdbMachine>, RootDebugger)> {
        let srv = GdbMachine::new("0.0.0.0:0", DummyDebugger::instance())?;
        srv.set_master();

        let srv = Arc::new(srv);

        let psrv = srv.clone();
        thread::spawn(move || {
            psrv.run().unwrap();
        });

        let rdbg = RootDebugger {
            state: srv.state.clone(),
            child_proc: None,
        };

        Ok((srv, rdbg))
    }

    fn do_cmd(
        dbg: Arc<Mutex<Box<dyn Debugger>>>,
        state: Option<Arc<Mutex<Box<dyn Debugger>>>>,
        cmd: &GdbMachineCommand,
    ) -> Option<GdbMachineResponse> {
        let mut dbg = dbg.lock().unwrap();

        match cmd {
            GdbMachineCommand::Start => Some(GdbMachineResponse::from_result(dbg.start())),
            GdbMachineCommand::Stop => Some(GdbMachineResponse::from_result(dbg.stop())),
            GdbMachineCommand::Continue => Some(GdbMachineResponse::from_result(dbg.cont())),
            GdbMachineCommand::GetState => Some(GdbMachineResponse::from_state(dbg.state())),
            GdbMachineCommand::GetSnapshot => {
                Some(GdbMachineResponse::snapshot_from_result(dbg.snapshot()))
            }
            GdbMachineCommand::GetSymbols => {
                Some(GdbMachineResponse::symbols_from_result(dbg.symbols()))
            }
            GdbMachineCommand::Count => Some(GdbMachineResponse::Count(dbg.count().unwrap_or(0))),
            GdbMachineCommand::Pivot(process_info, from) => {
                let ret = if let Some(state) = state {
                    let mut state = state.lock().unwrap();

                    let tree_state = state.as_mut().as_treestate().unwrap();

                    match tree_state.pivot(process_info, from.clone()) {
                        Ok((id, targ)) => Some(GdbMachineResponse::Pivot(id, targ)),
                        Err(e) => Some(GdbMachineResponse::Error(e.to_string())),
                    }
                } else {
                    None
                };
                ret
            }
            GdbMachineCommand::Join(target) => {
                let ret = if let Some(state) = state {
                    let mut state = state.lock().unwrap();
                    let tree_state = state.as_mut().as_treestate().unwrap();

                    match tree_state.join(target.clone()) {
                        Ok(()) => Some(GdbMachineResponse::Ok),
                        Err(e) => Some(GdbMachineResponse::Error(e.to_string())),
                    }
                } else {
                    None
                };
                ret
            }
        }
    }

    fn _run_command(
        dbg: Arc<Mutex<Box<dyn Debugger>>>,
        state: Arc<Mutex<Box<dyn Debugger>>>,
        cmd: GdbMachineCommand,
    ) -> GdbMachineResponse {
        let mut remote_result = None;
        let mut local_result = None;

        let st1 = state.clone();
        let st2 = state.clone();

        // Use rayon's scope to execute the tasks in parallel
        scope(|s| {
            s.spawn(|_| {
                // Execute the first command in a separate thread
                remote_result = GdbMachine::do_cmd(st1, None, &cmd);
            });

            s.spawn(|_| {
                // Execute the second command in a separate thread
                local_result = GdbMachine::do_cmd(dbg, Some(st2), &cmd);
            });
        });

        if let Some(local) = local_result {
            return TreeState::merge_results(local, remote_result);
        }

        GdbMachineResponse::Error("Local command did not return a response".to_string())
    }

    fn _client_loop(
        mut sock: TcpStream,
        dbg: Arc<Mutex<Box<dyn Debugger>>>,
        state: Arc<Mutex<Box<dyn Debugger>>>,
    ) -> Result<()> {
        loop {
            let resp = read_until_null(&mut sock)?;

            if resp.is_empty() {
                break;
            }

            log::debug!("INBOUND: {:?}", resp);

            let cmd: GdbMachineCommand = serde_json::from_str(&resp)?;

            let resp = GdbMachine::_run_command(dbg.clone(), state.clone(), cmd);

            log::debug!("OUTBOUND: {:?}", resp);

            let resp_json = serde_json::to_string(&resp)?;

            /* Write JSON */
            sock.write_all(resp_json.as_bytes())?;
            /* Write Separator */
            sock.write_all("\0".as_bytes())?;
            sock.flush()?;
        }

        Ok(())
    }

    pub fn run(&self) -> Result<()> {
        loop {
            let (stream, _) = self.listening_sock.accept()?;

            let dbg = self.dbg.clone();
            let state = self.state.clone();
            thread::spawn(move || match GdbMachine::_client_loop(stream, dbg, state) {
                Ok(_) => {}
                Err(e) => {
                    println!("Error processing client request : {}", e);
                }
            });
        }
    }

    pub fn url(&self) -> Result<String> {
        Ok(format!(
            "{}:{}",
            self.host,
            self.listening_sock.local_addr()?.port()
        ))
    }

    pub fn set_master(&self) {
        if let Ok(state) = self.state.lock().as_mut() {
            state.as_treestate().unwrap().set_root(self.url().unwrap());
        }
    }

    pub fn set_id(&self, id: u64) {
        if let Ok(state) = self.state.lock().as_mut() {
            state.set_id(id);
        }

        if let Ok(dbg) = self.dbg.lock().as_mut() {
            dbg.set_id(id);
        }
    }

    pub fn tree_count(&self) -> Option<usize> {
        let mut state = self.state.lock().unwrap();

        if let Some(st) = state.as_treestate() {
            if let Some(id) = st.id {
                if id == 0 {
                    return None;
                }
            }
        } else {
            return None;
        }

        /* We remove 1 as the root is self-pushed in this vec */
        Some(state.as_treestate().unwrap().seen_children.len() - 1)
    }
}

pub struct RootDebugger {
    state: Arc<Mutex<Box<dyn Debugger>>>,
    child_proc: Option<Child>,
}

impl RootDebugger {
    pub fn set_child(&mut self, child: Child) {
        self.child_proc = Some(child);
    }

    pub fn kill_child(&mut self) {
        if let Some(child) = &mut self.child_proc {
            let _ = child.kill();
        }
    }
}

impl Debugger for RootDebugger {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn set_id(&mut self, id: u64) {
        if let Ok(st) = self.state.lock().as_mut() {
            st.set_id(id);
        }
    }

    fn get_id(&self) -> u64 {
        match self.state.lock().as_mut() {
            Ok(st) => st.get_id(),
            Err(_) => 0,
        }
    }

    fn start(&mut self) -> Result<()> {
        match self.state.lock().as_mut() {
            Ok(st) => st.start(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }

    fn count(&mut self) -> Result<u64> {
        match self.state.lock().as_mut() {
            Ok(st) => st.count(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }

    fn stop(&mut self) -> Result<()> {
        match self.state.lock().as_mut() {
            Ok(st) => st.stop(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }

    fn cont(&mut self) -> Result<()> {
        match self.state.lock().as_mut() {
            Ok(st) => st.cont(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }

    fn state(&mut self) -> Result<HashMap<u64, RunState>> {
        match self.state.lock().as_mut() {
            Ok(st) => st.state(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }

    fn snapshot(&mut self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>> {
        match self.state.lock().as_mut() {
            Ok(st) => st.snapshot(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }

    fn symbols(&mut self) -> Result<SymbolTable> {
        match self.state.lock().as_mut() {
            Ok(st) => st.symbols(),
            Err(e) => Err(anyhow!(e.to_string())),
        }
    }
}
