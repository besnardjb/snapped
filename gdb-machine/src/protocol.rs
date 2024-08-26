use std::collections::HashMap;

use anyhow::anyhow;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::metadata::BacktraceState;
use crate::metadata::ProcessInfo;
use crate::metadata::RunState;
use crate::metadata::SymbolTable;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum GdbMachineCommand {
    Start,
    Count,
    Stop,
    Continue,
    GetState,
    GetSnapshot,
    GetSymbols,
    /* Process Info, Server Address */
    Pivot(ProcessInfo, String),
    Join(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum GdbMachineResponse {
    Error(String),
    Ok,
    State(HashMap<u64, RunState>),
    Snapshot(HashMap<u64, (u64, Vec<BacktraceState>)>),
    Symbols(SymbolTable),
    /* Returns Join URL and TreeDynamic */
    Pivot(u64, String),
    Count(u64),
}

impl GdbMachineResponse {
    pub fn ok(&self) -> Result<()> {
        match &self {
            GdbMachineResponse::Ok => Ok(()),
            GdbMachineResponse::Error(e) => Err(anyhow!("Error: {}", e)),
            _ => Err(anyhow!("This is not a return type")),
        }
    }

    pub fn from_state(res: Result<HashMap<u64, RunState>>) -> GdbMachineResponse {
        match res {
            Ok(st) => GdbMachineResponse::State(st),
            Err(e) => GdbMachineResponse::Error(e.to_string()),
        }
    }

    pub fn from_result(res: Result<()>) -> GdbMachineResponse {
        match res {
            Ok(_) => GdbMachineResponse::Ok,
            Err(e) => GdbMachineResponse::Error(e.to_string()),
        }
    }

    pub fn snapshot_from_result(
        ret: Result<HashMap<u64, (u64, Vec<BacktraceState>)>>,
    ) -> GdbMachineResponse {
        match ret {
            Ok(sn) => GdbMachineResponse::Snapshot(sn),
            Err(e) => GdbMachineResponse::Error(e.to_string()),
        }
    }

    pub fn symbols_from_result(ret: Result<SymbolTable>) -> GdbMachineResponse {
        match ret {
            Ok(st) => GdbMachineResponse::Symbols(st),
            Err(e) => GdbMachineResponse::Error(e.to_string()),
        }
    }

    pub fn state(self) -> HashMap<u64, RunState> {
        if let GdbMachineResponse::State(st) = self {
            return st;
        }

        unreachable!("This should only be called on a state response");
    }

    pub fn snapshot(self) -> Result<HashMap<u64, (u64, Vec<BacktraceState>)>> {
        if let GdbMachineResponse::Snapshot(sn) = self {
            return Ok(sn);
        }

        Err(anyhow!("Failed to retrieve snapshot from command"))
    }

    pub fn symbols(self) -> Result<SymbolTable> {
        if let GdbMachineResponse::Symbols(sy) = self {
            return Ok(sy);
        }

        Err(anyhow!("Failed to retrieve snapshot from command"))
    }
}
