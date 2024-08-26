use std::{collections::HashMap, io::Read, path::PathBuf, str::FromStr};

use anyhow::Result;
use ascii_tree::{write_tree, Tree};
use colored::*;
use gdb_machine::metadata::{BacktraceState, DisplayFrame, DisplayState};

fn line_from_src(spath: &Option<String>, line: &Option<u32>) -> Option<String> {
    if let (Some(spath), Some(line)) = (spath, line) {
        let path: PathBuf = PathBuf::from_str(spath).ok()?;

        if !path.is_file() {
            return None;
        }

        let f = std::fs::File::open(path).ok()?;
        let mut content = String::new();
        let mut reader = std::io::BufReader::new(&f);

        reader.read_to_string(&mut content).ok()?;

        let lines: Vec<&str> = content.split("\n").collect();

        if let Some(l) = lines.get((*line - 1) as usize) {
            let l = l.trim();
            if l.is_empty() {
                return None;
            }

            /* Gdb may return { at block start */
            if l == "{" {
                return lines.get(*line as usize).map(|v| v.to_string());
            }

            return Some(l.to_string());
        }
    }

    None
}

fn keep_file(fullpath: &Option<String>) -> Option<String> {
    if let Some(p) = fullpath {
        let ret = p.rsplit('/').next().unwrap_or(p);
        return Some(ret.to_string());
    }

    None
}

#[derive(Debug)]
pub struct FrameTree {
    pub frame: BacktraceState,
    pub counter: u64,
    pub child: HashMap<u64, FrameTree>,
}

impl FrameTree {
    fn default() -> FrameTree {
        FrameTree {
            frame: BacktraceState::root(),
            counter: 0,
            child: HashMap::new(),
        }
    }

    fn descriptor_frame(f: &DisplayFrame, allow_code: bool) -> String {
        let line = if let (Some(l), true) = (line_from_src(&f.file, &f.line), allow_code) {
            format!(" -> {}", l.bold().truecolor(100, 100, 100))
        } else {
            "".normal().to_string()
        };

        let func_str = f.func.to_string().cyan();
        let loc_str = if let (Some(f), Some(l)) = (keep_file(&f.file), &f.line) {
            format!(" {}:{}", f, l).magenta()
        } else {
            "".to_string().normal()
        };

        format!("{}{}{}", func_str, loc_str, line)
    }

    fn descriptor_stopstate(s: &DisplayState, _allow_code: bool) -> String {
        let reason = match s.reason.as_str() {
            "exited" => "Exited Badly".bright_yellow(),
            "exited-normally" => "Exited Normally".green(),
            "signal-received" => "Received a Signal".red(),
            other => other.red(),
        };

        format!("{}", reason.bold())
    }

    fn descriptor(&self, max_counter: u64, allow_code: bool) -> String {
        let intensity = if max_counter != 0 {
            let normalized = self.counter as f32 / max_counter as f32;

            let (r, g, b) = if normalized < 0.5 {
                // Transition from blue (0, 0, 255) to yellow (255, 255, 0)
                let t = normalized * 2.0;
                (
                    (t * 255.0) as u8, // Red increases
                    (t * 255.0) as u8, // Green increases
                    255,               // Blue remains constant
                )
            } else {
                // Transition from yellow (255, 255, 0) to red (255, 0, 0)
                let t = (normalized - 0.5) * 2.0;
                (
                    255,                       // Red remains constant
                    ((1.0 - t) * 255.0) as u8, // Green decreases
                    0,                         // Blue decreases to 0
                )
            };
            Some((r, g, b))
        } else {
            None
        };

        let counter_str = match intensity {
            Some((r, g, b)) => format!("{}", self.counter).truecolor(r, g, b),
            _ => format!("{}", self.counter).normal(),
        };

        let content = match &self.frame {
            BacktraceState::Frame(f) => FrameTree::descriptor_frame(f, allow_code),
            BacktraceState::State(s) => FrameTree::descriptor_stopstate(s, allow_code),
        };

        format!("{} {}", counter_str, content)
    }

    fn _display(&self, depth: usize) {
        let tabs = " ".to_string().repeat(depth);

        println!("{} {}", tabs, self.descriptor(0, true));

        for nxt in self.child.values() {
            nxt._display(depth + 1);
        }
    }

    #[allow(unused)]
    fn display(&self) {
        self._display(0);
    }

    fn _to_ascii_tree(&self, max_value: u64) -> Tree {
        if self.child.is_empty() {
            let mut content = vec![self.descriptor(max_value, false)];

            let cnt_len = self.counter.to_string().len() + 1;

            // Maybe move this in a dedicated function
            match &self.frame {
                BacktraceState::Frame(f) => {
                    if let Some(line) = line_from_src(&f.file, &f.line) {
                        content.push(format!(
                            "{}{}",
                            " ".repeat(cnt_len),
                            line.truecolor(180, 180, 180).bold()
                        ))
                    }
                }
                BacktraceState::State(s) => {
                    if let Some(sig) = &s.signal_name {
                        content.push(format!(
                            "{}{}",
                            " ".repeat(cnt_len),
                            sig.truecolor(180, 180, 180).bold()
                        ))
                    }
                    if let Some(exit_code) = &s.exit_code {
                        let exit = format!("Exit Code {}", exit_code);
                        content.push(format!(
                            "{}{}",
                            " ".repeat(cnt_len),
                            exit.truecolor(180, 180, 180).bold()
                        ))
                    }
                }
            }

            return Tree::Leaf(content);
        }

        let child = self
            .child
            .values()
            .map(|v| v._to_ascii_tree(max_value))
            .collect();

        Tree::Node(self.descriptor(max_value, true), child)
    }

    fn to_ascii_tree(&self) -> Tree {
        self._to_ascii_tree(self.counter)
    }
}

impl From<&BacktraceState> for FrameTree {
    fn from(value: &BacktraceState) -> Self {
        FrameTree {
            frame: value.clone(),
            counter: 0,
            child: HashMap::new(),
        }
    }
}

impl From<&HashMap<u64, (u64, Vec<BacktraceState>)>> for FrameTree {
    fn from(components: &HashMap<u64, (u64, Vec<BacktraceState>)>) -> Self {
        /* HASH to (contributors, Frames) */

        let mut root = FrameTree::default();

        /* Make sure root is visited as the number of backtraces */
        root.counter = components.values().map(|(cnt, _)| cnt).sum();

        let mut current_node = &mut root;

        for (counter, backtraces) in components.values() {
            for frame in backtraces.iter().rev() {
                current_node = current_node
                    .child
                    .entry(frame.get_hash())
                    .or_insert(FrameTree::from(frame));
                current_node.counter += counter;
            }
            /* Return to root */
            current_node = &mut root;
        }

        root
    }
}

pub struct Renderer {
    components: HashMap<u64, (u64, Vec<BacktraceState>)>,
}

impl Renderer {
    pub fn new(components: HashMap<u64, (u64, Vec<BacktraceState>)>) -> Renderer {
        Renderer { components }
    }

    fn astree(&self) -> FrameTree {
        FrameTree::from(&self.components)
    }

    pub fn print_tree(&self) -> Result<()> {
        let tree = self.astree();
        let ascii = tree.to_ascii_tree();

        let mut out = String::new();
        write_tree(&mut out, &ascii)?;

        println!("{}", out);

        Ok(())
    }
}
