use anyhow::{anyhow, Result};
use regex::Regex;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::str::FromStr;
use std::{cmp::max, collections::HashMap, io::Read};

pub fn dominating_numa_id() -> Result<u64> {
    let numa_maps = PathBuf::from_str("/proc/self/numa_maps")?;

    if !numa_maps.is_file() {
        return Err(anyhow!("Failed to read numa_maps"));
    }

    let numa_maps = File::open(numa_maps)?;
    let mut data = String::new();

    let mut r = BufReader::new(numa_maps);

    r.read_to_string(&mut data)?;

    /* Only keep heap lines */
    let data: String = data
        .lines()
        .into_iter()
        .filter(|v| v.contains(" heap "))
        .collect::<Vec<&str>>()
        .join("\n");

    let re = Regex::new("(N[0-9]+=[0-9]+)")?;

    let mut numa_count: HashMap<u64, u64> = HashMap::new();

    let captures = re.captures_iter(&data);

    for c in captures {
        if let Some(c) = c.get(1).and_then(|c| Some(c.as_str())) {
            if let Some(inner) = c.strip_prefix("N") {
                let spd: Vec<u64> = inner
                    .split("=")
                    .map(|v| v.parse::<u64>().unwrap_or(0))
                    .collect();

                if spd.len() == 2 {
                    let numa = spd[0];
                    let count = spd[1];

                    numa_count
                        .entry(numa)
                        .and_modify(|v| *v += count)
                        .or_insert(count);
                }
            }
        }
    }

    match numa_count.iter().max_by_key(|(_, v)| *v) {
        Some((k, _)) => Ok(*k),
        None => Err(anyhow!("Numa list is empty")),
    }
}

pub fn strdistance(a: &String, b: &String) -> u64 {
    let len = max(a.len(), b.len());

    let mut ret: u64 = 0;

    for i in 0..len {
        let va: u64 = a.chars().nth(i).and_then(|v| Some(v as u64)).unwrap_or(0);
        let vb: u64 = b.chars().nth(i).and_then(|v| Some(v as u64)).unwrap_or(0);

        ret += va.abs_diff(vb);
    }

    ret
}

pub fn parse_gdb_equal_list(list: &str) -> HashMap<String, String> {
    let re = Regex::new("([a-z\\-]+)=\"([^\"]+)\"").unwrap();

    let cap: Vec<(String, String)> = re
        .captures_iter(list)
        .flat_map(|b| {
            if let (Some(k), Some(v)) = (b.get(1), b.get(2)) {
                Some((k.as_str().to_string(), v.as_str().to_string()))
            } else {
                None
            }
        })
        .collect();
    HashMap::from_iter(cap)
}

pub fn extract_gdb_group(list: &str) -> Vec<String> {
    let re = Regex::new("\\{([^{}]*)\\}").unwrap();

    re.captures_iter(list)
        .flat_map(|v| v.get(1))
        .map(|v| v.as_str().to_string())
        .collect()
}

pub fn parse_response_with_token(marker: &str, resp: &str) -> Option<(u64, String)> {
    let re = Regex::new(format!("^([0-9]+){}(.*)\n", marker).as_str()).ok()?;

    let cap = re.captures(resp)?;

    if let (Some(id), Some(resp)) = (cap.get(1), cap.get(2)) {
        let id = id.as_str().parse::<u64>().ok()?;
        return Some((id, resp.as_str().to_string()));
    }

    None
}

pub fn gdb_output_to_json_repr(resp: &str) -> Result<String> {
    let re = Regex::new("([a-zA-Z]+)=")?;

    let resp_json = re.replace_all(resp, "\"$1\" : ");

    Ok(resp_json.to_string())
}

pub fn read_until_null(stream: &mut impl Read) -> Result<String> {
    let mut ret: String = String::new();

    loop {
        let mut data = [0; 1];
        match stream.read(&mut data) {
            Ok(0) => {
                return Ok(ret);
            }
            Ok(n) => {
                for i in 0..n {
                    if data[i] as char == '\0' {
                        return Ok(ret);
                    } else {
                        ret.push_str(std::str::from_utf8(&data[i..i + 1])?);
                    }
                }
            }
            Err(e) => {
                return Err(anyhow!(e));
            }
        }
    }
}
