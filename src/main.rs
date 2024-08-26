//! `Snapped` is a distributed program snapshoter relying on a TBON and
//! the GDB-MI (and thus an underlying GDB instance) to capture distributed
//! program states in a scalable manner.
//!
//! It is implemented to allow direct feedback on deadlocks or crashes.
//!
//!
//! # Direct Usage
//!
//! As gdb you can do:
//! `snapped ./a.out`
//!
//! # Parallel Usage
//!
//! But of course you can also run parallel programs:
//!
//! `snapped -p 4 mpirun -np 4 snapped ./a.out`
//!
//! See how you need to pass the number of process to pivot to the parent process, this information is used to build a topology-aware TBON.
//!
//! Naturally, the same syntax applies to parallel runs using `srun`:
//!
//! ̀`snapped -p 1000 srun -n 1000 -p rome ./snapped a.out`

use anyhow::{anyhow, Result};
use clap::Parser;
use colored::*;
use gdb_machine::debugger::Debugger;
use gdb_machine::{GdbMachine, RootDebugger};
use render::Renderer;
use std::process::{exit, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::{env, thread};

mod render;

static WAS_INTERRUPTED: Mutex<u32> = Mutex::new(0);

fn snap_log(out: &str) {
    println!("{} {}", "=SNAPPED=".bold().blue(), out);
}

fn interrupted() -> bool {
    let ret = if let Ok(st) = WAS_INTERRUPTED.lock() {
        *st > 0
    } else {
        false
    };

    ret
}

fn set_interrupted() {
    if let Ok(mut l) = WAS_INTERRUPTED.lock() {
        *l += 1;

        /* More than 3 times we exit */
        if *l > 3 {
            exit(1);
        }
    }
}

fn timeout(time: u32) {
    thread::spawn(move || {
        let mut current = 0;
        loop {
            if current >= time {
                set_interrupted();
                snap_log("Timeout reached");

                break;
            }
            thread::sleep(Duration::from_secs(1));
            current += 1;
        }
    });
}

#[derive(clap::Parser)]
struct Arguments {
    /// Shoud the program be interupted after a given number of seconds
    #[arg(short, long)]
    interrupt_after: Option<u32>,
    /// Shoud the program backconnect to a root debugger instance
    #[arg(short, long)]
    root_server: Option<String>,
    /// Should the program act as a GDB server
    #[arg(short, long)]
    pivot_processes: Option<usize>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Option<Vec<String>>,
}

fn timer_print(text: &str, start: Instant) {
    snap_log(&format!(
        "{} in {} seconds",
        text,
        start.elapsed().as_millis() as f64 / 1000.0
    ));
}

fn run_in_snapshot_mode(dbg: &mut impl Debugger) -> Result<()> {
    let bstart = Instant::now();
    dbg.start()?;
    timer_print("Started processes", bstart);

    loop {
        if !dbg.all_running()? || interrupted() {
            /* Stop all  */
            let bstop = Instant::now();
            dbg.stop()?;
            timer_print("Stopped processes", bstop);

            break;
        }
        thread::sleep(Duration::from_millis(500));
    }

    let bsnap = Instant::now();
    let snap = dbg.snapshot()?;
    timer_print("Collected backtraces", bsnap);

    let render = Renderer::new(snap);
    render.print_tree()?;

    Ok(())
}

fn be_root_server(child_count: usize, cmd: &Option<Vec<String>>) -> Result<RootDebugger> {
    let (srv, mut rdbg) = GdbMachine::run_as_root()?;

    snap_log(&format!("root server is running on {}", srv.url()?));

    if let Some(command) = cmd {
        env::set_var("GDBW_ROOT_SERVER", srv.url()?);

        let child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .spawn()?;
        rdbg.set_child(child);
    }
    let bstart = Instant::now();
    srv.wait_for_child(child_count)?;
    timer_print(
        &format!("Built a tree of {} processes", child_count),
        bstart,
    );

    snap_log("All processes joined root server");

    Ok(rdbg)
}

fn main() -> Result<()> {
    env_logger::init();

    ctrlc::set_handler(|| {
        set_interrupted();
    })?;

    let args = Arguments::parse();

    if let Some(time) = args.interrupt_after {
        timeout(time);
    }

    //if let Some(target) = args.target_server {}

    /* Get root server either from env */
    let root_server = if let Some(root) = args.root_server {
        Some(root)
    } else if let Ok(env) = env::var("GDBW_ROOT_SERVER") {
        Some(env)
    } else {
        None
    };

    if let Some(root) = root_server {
        if let Some(command) = &args.command {
            GdbMachine::run_as_leaf(root, command)?;
        } else {
            return Err(anyhow!(
                "You need to pass a command when running as non-root server"
            ));
        }
    }

    if let Some(count_proc) = args.pivot_processes {
        /* Server MODE */
        let mut srv = be_root_server(count_proc, &args.command)?;
        run_in_snapshot_mode(&mut srv)?;
        srv.kill_child();
    } else if let Some(cmd) = &args.command {
        /* If we are here we are not doing Client / Server we launch locally */
        let mut dbg = GdbMachine::local(cmd)?;
        run_in_snapshot_mode(&mut dbg)?;
        dbg.kill_child();
    }

    Ok(())
}
