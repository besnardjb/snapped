use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    metadata::{BacktraceState, RunState, SymbolTable},
    TreeState,
};
use anyhow::{anyhow, Result};

pub trait Debugger: Send + Any {
    /// Number of attached debuggers
    fn count(&mut self) -> Result<u64>;

    /// Set debugger ID (default is 0)
    fn set_id(&mut self, id: u64);

    /// Get the debugger id
    fn get_id(&self) -> u64;

    /// Start the debugged program
    fn start(&mut self) -> Result<()>;
    /// Stop the debugged program
    fn stop(&mut self) -> Result<()>;
    /// Continue a stopped program
    fn cont(&mut self) -> Result<()>;

    /// Get current state of program
    fn state(&mut self) -> Result<HashMap<u64, RunState>>;

    /// Check if the given `id` has exited (program terminated)
    fn id_is_exited(&mut self, id: u64) -> Result<bool> {
        let st = self.state()?;

        if let Some(st) = st.get(&id) {
            if let RunState::Stopped(st) = st {
                if st.exited() {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// Check if the given `id` is running (not stopped or exited)
    fn id_is_running(&mut self, id: u64) -> Result<bool> {
        let st = self.state()?;

        if let Some(st) = st.get(&id) {
            if matches!(st, RunState::Running(_)) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Check if the given `id` is stopped (not running)
    fn id_is_stopped(&mut self, id: u64) -> Result<bool> {
        let st = self.state()?;

        if let Some(st) = st.get(&id) {
            if matches!(st, RunState::Stopped(_)) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Is the program still in execution
    /// Returns false is the debuggee is still runing
    /// it means the process has not ended
    /// This is not to be confused with the `isrunning` state
    /// which means the program is being executed (you need to `stop` first)
    fn isexited(&mut self) -> Result<HashMap<u64, bool>> {
        Ok(self
            .state()?
            .iter()
            .map(|(k, v)| {
                let running = if let RunState::Stopped(st) = v {
                    st.exited()
                } else {
                    false
                };

                (*k, running)
            })
            .collect())
    }

    /// Check if processes are running returns individual processes state
    fn isrunning(&mut self) -> Result<HashMap<u64, bool>> {
        Ok(self
            .state()?
            .iter()
            .map(|(k, v)| {
                let running = if let RunState::Running(_) = v {
                    true
                } else {
                    false
                };

                (*k, running)
            })
            .collect())
    }

    /// Check if all processes are running (returns a global bool)
    fn all_running(&mut self) -> Result<bool> {
        let runs = self.isrunning()?;

        for r in runs.values() {
            if !r {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Is the program stoped
    fn isstopped(&mut self) -> Result<HashMap<u64, bool>> {
        Ok(self
            .state()?
            .iter()
            .map(|(k, v)| {
                let running = if let RunState::Stopped(_) = v {
                    true
                } else {
                    false
                };

                (*k, running)
            })
            .collect())
    }

    /// Snapshot a stopped program
    fn snapshot(&mut self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>>;

    /// Get Symbol table
    fn symbols(&mut self) -> Result<SymbolTable>;

    fn as_any_mut(&mut self) -> &mut dyn Any;

    // New method to downcast to TreeState
    fn as_treestate(&mut self) -> Option<&mut TreeState> {
        self.as_any_mut().downcast_mut::<TreeState>()
    }
}

pub struct DummyDebugger;

impl Debugger for DummyDebugger {
    fn count(&mut self) -> Result<u64> {
        Ok(0)
    }

    #[allow(unused)]
    fn set_id(&mut self, id: u64) {}

    fn get_id(&self) -> u64 {
        0
    }

    /// Start the debugged program
    fn start(&mut self) -> Result<()> {
        Err(anyhow!("Dummy debugger"))
    }
    /// Stop the debugged program
    fn stop(&mut self) -> Result<()> {
        Err(anyhow!("Dummy debugger"))
    }
    /// Continue a stopped program
    fn cont(&mut self) -> Result<()> {
        Err(anyhow!("Dummy debugger"))
    }

    /// Get current state of program
    fn state(&mut self) -> Result<HashMap<u64, RunState>> {
        Ok(HashMap::new())
    }

    /// Snapshot a stopped program
    fn snapshot(&mut self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>> {
        Ok(HashMap::new())
    }

    /// Get Symbol table
    fn symbols(&mut self) -> Result<SymbolTable> {
        Err(anyhow!("Dummy debugger"))
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl DummyDebugger {
    pub fn instance() -> Arc<Mutex<Box<dyn Debugger>>> {
        let dbg: Arc<Mutex<Box<dyn Debugger>>> = Arc::new(Mutex::new(Box::new(DummyDebugger)));
        dbg
    }
}
