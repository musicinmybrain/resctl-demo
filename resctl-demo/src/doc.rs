// Copyright (c) Facebook, Inc. and its affiliates.
use cursive::direction::Orientation;
use cursive::utils::markup::StyledString;
use cursive::view::{Nameable, Resizable, Scrollable, SizeConstraint, View};
use cursive::views::{Button, Checkbox, Dialog, DummyView, LinearLayout, SliderView, TextView};
use cursive::Cursive;
use log::{error, info, warn};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Mutex, RwLock};

mod index;
mod markup_rd;

use super::agent::AGENT_FILES;
use super::command::{CmdState, CMD_STATE};
use super::graph::{clear_main_graph, set_main_graph, GraphTag};
use super::{get_layout, COLOR_ACTIVE, COLOR_ALERT};
use markup_rd::{RdCmd, RdDoc, RdKnob, RdPara, RdReset, RdSwitch};
use rd_agent_intf::{Cmd, HashdCmd, SliceConfig, SysReq};
use rd_util::*;

lazy_static::lazy_static! {
    pub static ref DOCS: BTreeMap<String, &'static str> = load_docs();
    pub static ref CUR_DOC: RwLock<RdDoc> = RwLock::new(RdDoc {
        id: "".into(),
        ..Default::default()
    });
    pub static ref DOC_HIST: Mutex<Vec<String>> = Mutex::new(Vec::new());
    pub static ref SIDELOAD_NAMES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    pub static ref SYSLOAD_NAMES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
}

fn load_docs() -> BTreeMap<String, &'static str> {
    let mut docs = BTreeMap::new();
    let mut graphs = HashSet::new();
    let mut targets = HashSet::new();

    for i in 0..index::SOURCES.len() {
        let src = index::SOURCES[i];
        info!("Loading doc {}", i);
        let doc = match RdDoc::parse(src.as_bytes()) {
            Ok(v) => v,
            Err(e) => panic!("Failed to load {:?}... ({:?})", &src[..100], &e),
        };

        let mut register_one_cmd = |cmd: &RdCmd| match cmd {
            RdCmd::On(sw) | RdCmd::Toggle(sw) => match sw {
                RdSwitch::Sideload(tag, _id) => {
                    SIDELOAD_NAMES.lock().unwrap().insert(tag.into());
                }
                RdSwitch::Sysload(tag, _id) => {
                    SYSLOAD_NAMES.lock().unwrap().insert(tag.into());
                }
                _ => {}
            },
            RdCmd::Graph(tag) => {
                if tag.len() > 0 {
                    graphs.insert(tag.clone());
                }
            }
            RdCmd::Jump(t) => {
                targets.insert(t.to_string());
            }
            _ => {}
        };

        for cmd in doc
            .pre_cmds
            .iter()
            .chain(doc.body.iter().filter_map(|para| {
                if let RdPara::Prompt(_, cmd) = para {
                    Some(cmd)
                } else {
                    None
                }
            }))
            .chain(doc.post_cmds.iter())
        {
            if let RdCmd::Group(group) = cmd {
                for cmd in group {
                    register_one_cmd(cmd);
                }
            } else {
                register_one_cmd(cmd);
            }
        }

        docs.insert(doc.id.clone(), src);
    }

    info!("SIDELOAD_NAMES: {:?}", &SIDELOAD_NAMES.lock().unwrap());
    info!("SYSLOAD_NAMES: {:?}", &SYSLOAD_NAMES.lock().unwrap());

    let mut nr_missing = 0;

    let graph_tags: HashSet<String> = enum_iterator::all::<GraphTag>()
        .map(|x| format!("{:?}", x))
        .collect();
    for tag in graphs.iter() {
        if !graph_tags.contains(tag) {
            error!("doc: invalid graph tag {:?}", tag);
            nr_missing += 1;
        }
    }

    for t in targets {
        if !docs.contains_key(&t) {
            error!("doc: invalid jump target {:?}", t);
            nr_missing += 1;
        }
    }

    assert!(nr_missing == 0);
    docs
}

fn format_markup_tags(tag: &str) -> Option<StyledString> {
    AGENT_FILES.refresh();
    let sysreqs = AGENT_FILES.sysreqs();
    let bench = AGENT_FILES.bench();
    let empty_some = Some(StyledString::plain(""));

    if tag.starts_with("SysReq::") {
        for req in enum_iterator::all::<SysReq>() {
            if format!("{:?}", req) == tag[8..] {
                if sysreqs.satisfied.contains(&req) {
                    return Some(StyledString::styled(tag, *COLOR_ACTIVE));
                } else {
                    return Some(StyledString::styled(tag, *COLOR_ALERT));
                }
            }
        }
    } else {
        match tag {
            "MissedSysReqs" => {
                let missed = sysreqs.missed.map.len();
                if missed > 0 {
                    return Some(StyledString::plain(format!("{}", missed)));
                } else {
                    return None;
                }
            }
            "NeedBenchHashd" => {
                if bench.hashd_seq > 0 {
                    return None;
                } else {
                    return empty_some;
                }
            }
            "NeedBenchIoCost" => {
                if bench.iocost_seq > 0 {
                    return None;
                } else {
                    return empty_some;
                }
            }
            "NeedBench" => {
                if bench.hashd_seq > 0 && bench.iocost_seq > 0 {
                    return None;
                } else {
                    return empty_some;
                }
            }
            "HaveBench" => {
                if bench.hashd_seq > 0 && bench.iocost_seq > 0 {
                    return empty_some;
                } else {
                    return None;
                }
            }
            "BenchBalloonSize" => {
                return Some(StyledString::plain(format_size(
                    Cmd::default().bench_hashd_balloon_size,
                )));
            }
            "HashdMemSize" => {
                return Some(StyledString::plain(format_size(
                    bench.hashd.mem_size as f64 * bench.hashd.mem_frac,
                )));
            }
            _ => {}
        }
    }

    Some(StyledString::plain(format!("%{}%", tag)))
}

fn exec_one_cmd(siv: &mut Cursive, cmd: &RdCmd) {
    info!("executing {:?}", cmd);

    let mut cs = CMD_STATE.lock().unwrap();
    let wbps = AGENT_FILES.bench().iocost.model.wbps as f64;

    match cmd {
        RdCmd::On(sw) | RdCmd::Off(sw) => {
            let is_on = if let RdCmd::On(_) = cmd { true } else { false };

            if is_on {
                // sync so that we don't clobber a preceding off command
                if let Err(e) = cs.sync() {
                    warn!("failed to wait for command ack ({:?})", &e);
                }
            }

            match sw {
                RdSwitch::BenchHashd => {
                    cs.bench_hashd_next = cs.bench_hashd_cur + if is_on { 1 } else { 0 };
                }
                RdSwitch::BenchHashdLoop => {
                    cs.bench_hashd_next = if is_on {
                        std::u64::MAX
                    } else {
                        cs.bench_hashd_cur
                    };
                }
                RdSwitch::BenchIoCost => {
                    cs.bench_iocost_next = cs.bench_iocost_cur + if is_on { 1 } else { 0 };
                }
                RdSwitch::BenchNeeded => {
                    if cs.bench_hashd_cur == 0 {
                        cs.bench_hashd_next = 1;
                    }
                    if cs.bench_iocost_cur == 0 {
                        cs.bench_iocost_next = 1;
                    }
                }
                RdSwitch::HashdA => cs.hashd[0].active = is_on,
                RdSwitch::HashdB => cs.hashd[1].active = is_on,
                RdSwitch::Sideload(tag, id) => {
                    if is_on {
                        cs.sideloads.insert(tag.clone(), id.clone());
                    } else {
                        cs.sideloads.remove(tag);
                    }
                }
                RdSwitch::Sysload(tag, id) => {
                    if is_on {
                        cs.sysloads.insert(tag.clone(), id.clone());
                    } else {
                        cs.sysloads.remove(tag);
                    }
                }
                RdSwitch::CpuResCtl => cs.cpu = is_on,
                RdSwitch::MemResCtl => cs.mem = is_on,
                RdSwitch::IoResCtl => cs.io = is_on,
                RdSwitch::Oomd => cs.oomd = is_on,
                RdSwitch::OomdWorkMemPressure => cs.oomd_work_mempress = is_on,
                RdSwitch::OomdWorkSenpai => cs.oomd_work_senpai = is_on,
                RdSwitch::OomdSysMemPressure => cs.oomd_sys_mempress = is_on,
                RdSwitch::OomdSysSenpai => cs.oomd_sys_senpai = is_on,
            }
        }
        RdCmd::Knob(knob, val) => match knob {
            RdKnob::HashdALoad => cs.hashd[0].rps_target_ratio = *val,
            RdKnob::HashdBLoad => cs.hashd[1].rps_target_ratio = *val,
            RdKnob::HashdALatTargetPct => cs.hashd[0].lat_target_pct = *val,
            RdKnob::HashdBLatTargetPct => cs.hashd[1].lat_target_pct = *val,
            RdKnob::HashdALatTarget => cs.hashd[0].lat_target = *val,
            RdKnob::HashdBLatTarget => cs.hashd[1].lat_target = *val,
            RdKnob::HashdAMem => cs.hashd[0].mem_ratio = Some(*val),
            RdKnob::HashdBMem => cs.hashd[1].mem_ratio = Some(*val),
            RdKnob::HashdAFileAddrStdev => {
                cs.hashd[0].file_addr_stdev = Some(if *val < 1.0 { *val } else { 100.0 });
            }
            RdKnob::HashdAAnonAddrStdev => {
                cs.hashd[0].anon_addr_stdev = Some(if *val < 1.0 { *val } else { 100.0 });
            }
            RdKnob::HashdBFileAddrStdev => {
                cs.hashd[1].file_addr_stdev = Some(if *val < 1.0 { *val } else { 100.0 });
            }
            RdKnob::HashdBAnonAddrStdev => {
                cs.hashd[1].anon_addr_stdev = Some(if *val < 1.0 { *val } else { 100.0 });
            }
            RdKnob::HashdAFile => cs.hashd[0].file_ratio = *val,
            RdKnob::HashdBFile => cs.hashd[1].file_ratio = *val,
            RdKnob::HashdAFileMax => cs.hashd[0].file_max_ratio = *val,
            RdKnob::HashdBFileMax => cs.hashd[1].file_max_ratio = *val,
            RdKnob::HashdALogBps => cs.hashd[0].log_bps = (wbps * *val).round() as u64,
            RdKnob::HashdBLogBps => cs.hashd[1].log_bps = (wbps * *val).round() as u64,
            RdKnob::HashdAWeight => cs.hashd[0].weight = *val,
            RdKnob::HashdBWeight => cs.hashd[1].weight = *val,
            RdKnob::SysCpuRatio => cs.sys_cpu_ratio = *val,
            RdKnob::SysIoRatio => cs.sys_io_ratio = *val,
            RdKnob::MemMargin => cs.mem_margin = *val,
            RdKnob::Balloon => cs.balloon_ratio = *val,
            RdKnob::CpuHeadroom => cs.cpu_headroom = *val,
        },
        RdCmd::Graph(tag_name) => {
            if tag_name.len() > 0 {
                let tag = enum_iterator::all::<GraphTag>()
                    .filter(|x| &format!("{:?}", x) == tag_name)
                    .next()
                    .unwrap();
                set_main_graph(siv, tag);
            } else {
                clear_main_graph(siv);
            }
        }
        RdCmd::Reset(reset) => {
            let reset_benches = |cs: &mut CmdState| {
                cs.bench_hashd_next = cs.bench_hashd_cur;
                cs.bench_iocost_next = cs.bench_iocost_cur;
            };
            let reset_hashds = |cs: &mut CmdState| {
                cs.hashd[0].active = false;
                cs.hashd[1].active = false;
            };
            let reset_hashd_params = |cs: &mut CmdState| {
                cs.hashd[0] = HashdCmd {
                    active: cs.hashd[0].active,
                    ..Default::default()
                };
                cs.hashd[1] = HashdCmd {
                    active: cs.hashd[1].active,
                    ..Default::default()
                };
            };
            let reset_secondaries = |cs: &mut CmdState| {
                cs.sideloads.clear();
                cs.sysloads.clear();
            };
            let reset_resctl = |cs: &mut CmdState| {
                cs.cpu = true;
                cs.mem = true;
                cs.io = true;
            };
            let reset_resctl_params = |cs: &mut CmdState| {
                let dfl_cmd = Cmd::default();

                cs.sys_cpu_ratio = SliceConfig::DFL_SYS_CPU_RATIO;
                cs.sys_io_ratio = SliceConfig::DFL_SYS_IO_RATIO;
                cs.mem_margin = SliceConfig::dfl_mem_margin(total_memory(), *IS_FB_PROD) as f64
                    / total_memory() as f64;
                cs.balloon_ratio = dfl_cmd.balloon_ratio;
                cs.cpu_headroom = dfl_cmd.sideloader.cpu_headroom;
            };
            let reset_oomd = |cs: &mut CmdState| {
                cs.oomd = true;
                cs.oomd_work_mempress = true;
                cs.oomd_work_senpai = false;
                cs.oomd_sys_mempress = true;
                cs.oomd_sys_senpai = false;
            };
            let reset_graph = |siv: &mut Cursive| {
                clear_main_graph(siv);
            };
            let reset_all = |cs: &mut CmdState, siv: &mut Cursive| {
                reset_benches(cs);
                reset_hashds(cs);
                reset_secondaries(cs);
                reset_resctl(cs);
                reset_oomd(cs);
                reset_graph(siv);
            };
            let reset_prep = |cs: &mut CmdState, siv: &mut Cursive| {
                reset_secondaries(cs);
                reset_resctl(cs);
                reset_oomd(cs);
                reset_hashd_params(cs);
                reset_resctl_params(cs);
                reset_graph(siv);
            };

            match reset {
                RdReset::Benches => reset_benches(&mut cs),
                RdReset::Hashds => reset_hashds(&mut cs),
                RdReset::HashdParams => reset_hashd_params(&mut cs),
                RdReset::Sideloads => cs.sideloads.clear(),
                RdReset::Sysloads => cs.sysloads.clear(),
                RdReset::ResCtl => reset_resctl(&mut cs),
                RdReset::ResCtlParams => reset_resctl_params(&mut cs),
                RdReset::Oomd => reset_oomd(&mut cs),
                RdReset::Graph => reset_graph(siv),
                RdReset::Secondaries => reset_secondaries(&mut cs),
                RdReset::AllWorkloads => {
                    reset_hashds(&mut cs);
                    reset_secondaries(&mut cs);
                }
                RdReset::Protections => {
                    reset_resctl(&mut cs);
                    reset_oomd(&mut cs);
                }
                RdReset::All => {
                    reset_all(&mut cs, siv);
                }
                RdReset::Params => {
                    reset_hashd_params(&mut cs);
                    reset_resctl_params(&mut cs);
                }
                RdReset::AllWithParams => {
                    reset_all(&mut cs, siv);
                    reset_hashd_params(&mut cs);
                    reset_resctl_params(&mut cs);
                }
                RdReset::Prep => {
                    reset_prep(&mut cs, siv);
                }
            }
        }
        _ => panic!("exec_cmd: unexpected command {:?}", cmd),
    }

    if let Err(e) = cs.apply() {
        error!("failed to apply {:?} cmd ({})", cmd, &e);
    }

    drop(cs);
    refresh_cur_doc(siv);
}

fn exec_cmd(siv: &mut Cursive, cmd: &RdCmd) {
    if let RdCmd::Group(group) = cmd {
        for cmd in group {
            exec_one_cmd(siv, cmd);
        }
    } else {
        exec_one_cmd(siv, cmd);
    }
}

fn exec_toggle(siv: &mut Cursive, cmd: &RdCmd, val: bool) {
    if let RdCmd::Toggle(sw) = cmd {
        let new_cmd = match val {
            true => RdCmd::On(sw.clone()),
            false => RdCmd::Off(sw.clone()),
        };
        exec_cmd(siv, &new_cmd);
    } else {
        panic!();
    }
}

fn format_knob_val(knob: &RdKnob, ratio: f64) -> String {
    let bench = AGENT_FILES.bench();

    let v = match knob {
        RdKnob::HashdALatTarget | RdKnob::HashdBLatTarget => {
            format!("{}m", (ratio * 1000.0).round())
        }
        RdKnob::HashdAMem | RdKnob::HashdBMem => format_size(ratio * bench.hashd.mem_size as f64),
        RdKnob::HashdALogBps | RdKnob::HashdBLogBps => {
            format_size(ratio * bench.iocost.model.wbps as f64)
        }
        RdKnob::MemMargin => format_size(ratio * total_memory() as f64),
        RdKnob::Balloon => format_size(ratio * total_memory() as f64),
        _ => format4_pct(ratio) + "%",
    };

    format!("{:>5}", &v)
}

fn exec_knob(siv: &mut Cursive, cmd: &RdCmd, val: usize, range: usize) {
    if let RdCmd::Knob(knob, _) = cmd {
        let ratio = val as f64 / (range - 1) as f64;
        siv.call_on_all_named(&format!("{:?}-digit", knob), |t: &mut TextView| {
            t.set_content(format_knob_val(knob, ratio))
        });
        let new_cmd = RdCmd::Knob(knob.clone(), ratio);
        exec_cmd(siv, &new_cmd);
    } else {
        panic!();
    }
}

fn refresh_toggles(siv: &mut Cursive, doc: &RdDoc, cs: &CmdState) {
    for sw in doc.toggles.iter() {
        let val = match sw {
            RdSwitch::BenchHashd => cs.bench_hashd_next > cs.bench_hashd_cur,
            RdSwitch::BenchHashdLoop => cs.bench_hashd_next == std::u64::MAX,
            RdSwitch::BenchIoCost => cs.bench_iocost_next > cs.bench_iocost_cur,
            RdSwitch::BenchNeeded => cs.bench_hashd_cur == 0 || cs.bench_iocost_cur == 0,
            RdSwitch::HashdA => cs.hashd[0].active,
            RdSwitch::HashdB => cs.hashd[1].active,
            RdSwitch::Sideload(tag, _) => cs.sideloads.contains_key(tag),
            RdSwitch::Sysload(tag, _) => cs.sysloads.contains_key(tag),
            RdSwitch::CpuResCtl => cs.cpu,
            RdSwitch::MemResCtl => cs.mem,
            RdSwitch::IoResCtl => cs.io,
            RdSwitch::Oomd => cs.oomd,
            RdSwitch::OomdWorkMemPressure => cs.oomd_work_mempress,
            RdSwitch::OomdWorkSenpai => cs.oomd_work_senpai,
            RdSwitch::OomdSysMemPressure => cs.oomd_sys_mempress,
            RdSwitch::OomdSysSenpai => cs.oomd_sys_senpai,
        };

        let name = match sw {
            RdSwitch::Sideload(tag, _) => {
                format!("{:?}", RdSwitch::Sideload(tag.into(), "ID".into()))
            }
            RdSwitch::Sysload(tag, _) => {
                format!("{:?}", RdSwitch::Sysload(tag.into(), "ID".into()))
            }
            _ => format!("{:?}", sw),
        };

        siv.call_on_all_named(&name, |c: &mut Checkbox| {
            c.set_checked(val);
        });
    }
}

fn refresh_one_knob(siv: &mut Cursive, knob: &RdKnob, mut val: f64) {
    val = val.max(0.0).min(1.0);
    siv.call_on_all_named(&format!("{:?}-digit", &knob), |t: &mut TextView| {
        t.set_content(format_knob_val(&knob, val))
    });
    siv.call_on_all_named(&format!("{:?}-slider", &knob), |s: &mut SliderView| {
        let range = s.get_max_value();
        let slot = (val * (range - 1) as f64).round() as usize;
        s.set_value(slot);
    });
}

fn hmem_ratio(knob: Option<f64>) -> f64 {
    match knob {
        Some(v) => v,
        None => AGENT_FILES.bench().hashd.mem_frac,
    }
}

fn hashd_cmd_file_addr_stdev(hashd: &HashdCmd) -> f64 {
    if let Some(v) = hashd.file_addr_stdev {
        v.min(1.0)
    } else {
        rd_hashd_intf::Params::default().file_addr_stdev_ratio
    }
}

fn hashd_cmd_anon_addr_stdev(hashd: &HashdCmd) -> f64 {
    if let Some(v) = hashd.anon_addr_stdev {
        v.min(1.0)
    } else {
        rd_hashd_intf::Params::default().anon_addr_stdev_ratio
    }
}

fn refresh_knobs(siv: &mut Cursive, doc: &RdDoc, cs: &CmdState) {
    let wbps = AGENT_FILES.bench().iocost.model.wbps as f64;

    for knob in doc.knobs.iter() {
        let val = match knob {
            RdKnob::HashdALoad => cs.hashd[0].rps_target_ratio,
            RdKnob::HashdBLoad => cs.hashd[1].rps_target_ratio,
            RdKnob::HashdALatTargetPct => cs.hashd[0].lat_target_pct,
            RdKnob::HashdBLatTargetPct => cs.hashd[1].lat_target_pct,
            RdKnob::HashdALatTarget => cs.hashd[0].lat_target,
            RdKnob::HashdBLatTarget => cs.hashd[1].lat_target,
            RdKnob::HashdAMem => hmem_ratio(cs.hashd[0].mem_ratio),
            RdKnob::HashdBMem => hmem_ratio(cs.hashd[1].mem_ratio),
            RdKnob::HashdAFileAddrStdev => hashd_cmd_file_addr_stdev(&cs.hashd[0]),
            RdKnob::HashdAAnonAddrStdev => hashd_cmd_anon_addr_stdev(&cs.hashd[0]),
            RdKnob::HashdBFileAddrStdev => hashd_cmd_file_addr_stdev(&cs.hashd[1]),
            RdKnob::HashdBAnonAddrStdev => hashd_cmd_anon_addr_stdev(&cs.hashd[1]),
            RdKnob::HashdAFile => cs.hashd[0].file_ratio,
            RdKnob::HashdBFile => cs.hashd[1].file_ratio,
            RdKnob::HashdAFileMax => cs.hashd[0].file_max_ratio,
            RdKnob::HashdBFileMax => cs.hashd[1].file_max_ratio,
            RdKnob::HashdALogBps => cs.hashd[0].log_bps as f64 / wbps,
            RdKnob::HashdBLogBps => cs.hashd[1].log_bps as f64 / wbps,
            RdKnob::HashdAWeight => cs.hashd[0].weight,
            RdKnob::HashdBWeight => cs.hashd[1].weight,
            RdKnob::SysCpuRatio => cs.sys_cpu_ratio,
            RdKnob::SysIoRatio => cs.sys_io_ratio,
            RdKnob::MemMargin => cs.mem_margin,
            RdKnob::Balloon => cs.balloon_ratio,
            RdKnob::CpuHeadroom => cs.cpu_headroom,
        };

        refresh_one_knob(siv, knob, val);
    }
}

fn refresh_cur_doc(siv: &mut Cursive) {
    let mut cmd_state = CMD_STATE.lock().unwrap();
    let cur_doc = CUR_DOC.read().unwrap();

    cmd_state.refresh();
    refresh_toggles(siv, &cur_doc, &cmd_state);
    refresh_knobs(siv, &cur_doc, &cmd_state);
}

pub fn show_doc(siv: &mut Cursive, target: &str, jump: bool, back: bool) {
    let doc = RdDoc::parse(DOCS.get(target).unwrap().as_bytes()).unwrap();
    let cur_doc = CUR_DOC.read().unwrap();

    if jump {
        for cmd in &cur_doc.post_cmds {
            exec_cmd(siv, cmd);
        }

        info!("doc: jumping to {:?}", target);

        for cmd in &doc.pre_cmds {
            if let RdCmd::Jump(target) = cmd {
                drop(cur_doc);
                show_doc(siv, target, true, false);
                return;
            }
            exec_cmd(siv, cmd);
        }

        if !back && cur_doc.id.len() > 0 {
            DOC_HIST.lock().unwrap().push(cur_doc.id.clone());
        }
    }

    drop(cur_doc);
    let mut cur_doc = CUR_DOC.write().unwrap();
    *cur_doc = doc;

    siv.call_on_name("doc", |d: &mut Dialog| {
        d.set_title(format!(
            "[{}] {} - 'i': index, 'b': back",
            &cur_doc.id, &cur_doc.desc
        ));
        d.set_content(render_doc(&cur_doc));
    });

    drop(cur_doc);
    refresh_cur_doc(siv);
}

fn create_button<F>(prompt: &str, cb: F) -> impl View
where
    F: 'static + Fn(&mut Cursive) + std::marker::Sync + std::marker::Send,
{
    let trimmed = prompt.trim_start();
    let indent = &prompt[0..prompt.len() - trimmed.len()];
    LinearLayout::horizontal()
        .child(TextView::new(indent))
        .child(Button::new_raw(trimmed, cb))
}

fn render_cmd(prompt: &str, cmd: &RdCmd) -> impl View {
    let width = get_layout().doc.x - 2;
    let mut view = LinearLayout::horizontal();
    let cmdc = cmd.clone();

    match cmd {
        RdCmd::On(_) | RdCmd::Off(_) => {
            view = view.child(create_button(prompt, move |siv| exec_cmd(siv, &cmdc)));
        }
        RdCmd::Toggle(sw) => {
            let name = match sw {
                RdSwitch::Sideload(tag, _id) => {
                    format!("{:?}", RdSwitch::Sideload(tag.into(), "ID".into()))
                }
                RdSwitch::Sysload(tag, _id) => {
                    format!("{:?}", RdSwitch::Sysload(tag.into(), "ID".into()))
                }
                _ => format!("{:?}", sw),
            };

            view = view.child(
                LinearLayout::horizontal()
                    .child(
                        Checkbox::new()
                            .on_change(move |siv, val| exec_toggle(siv, &cmdc, val))
                            .with_name(&name),
                    )
                    .child(DummyView)
                    .child(TextView::new(prompt)),
            );
        }
        RdCmd::Knob(knob, val) => {
            if *val < 0.0 {
                let digit_name = format!("{:?}-digit", knob);
                let slider_name = format!("{:?}-slider", knob);
                let range = (width as i32 - prompt.len() as i32 - 13).max(5) as usize;
                view = view.child(
                    LinearLayout::horizontal()
                        .child(TextView::new(prompt))
                        .child(DummyView)
                        .child(TextView::new(format_knob_val(knob, 0.0)).with_name(digit_name))
                        .child(TextView::new(" ["))
                        .child(
                            SliderView::new(Orientation::Horizontal, range)
                                .on_change(move |siv, val| exec_knob(siv, &cmdc, val, range))
                                .with_name(slider_name),
                        )
                        .child(TextView::new("]")),
                );
            } else {
                view = view.child(create_button(prompt, move |siv| exec_cmd(siv, &cmdc)));
            }
        }
        RdCmd::Graph(_) | RdCmd::Reset(_) | RdCmd::Group(_) => {
            view = view.child(create_button(prompt, move |siv| exec_cmd(siv, &cmdc)));
        }
        RdCmd::Jump(target) => {
            let t = target.clone();
            view = view.child(create_button(prompt, move |siv| {
                show_doc(siv, &t, true, false)
            }));
        }
        _ => panic!("invalid cmd {:?} for prompt {:?}", cmd, prompt),
    }
    view
}

fn render_doc(doc: &RdDoc) -> impl View {
    let mut view = LinearLayout::vertical();
    let mut prev_was_text = true;

    for para in &doc.body {
        match para {
            RdPara::Text(indent_opt, text) => {
                view = if prev_was_text {
                    view.child(LinearLayout::horizontal().child(Button::new_raw(" ", |_| {})))
                } else {
                    view.child(DummyView)
                };
                view = match indent_opt {
                    Some(indent) => view.child(
                        LinearLayout::horizontal()
                            .child(TextView::new(indent))
                            .child(TextView::new(text.clone())),
                    ),
                    None => view.child(TextView::new(text.clone())),
                };
                prev_was_text = !text.is_empty();
            }
            RdPara::Prompt(prompt, cmd) => {
                if prev_was_text {
                    view = view.child(DummyView);
                }
                view = view.child(render_cmd(prompt, cmd));
                prev_was_text = false;
            }
        }
    }
    view.scrollable()
        .show_scrollbars(true)
        .with_name("doc-scroll")
}

pub fn layout_factory() -> impl View {
    let layout = get_layout();

    Dialog::around(TextView::new("Loading document..."))
        .with_name("doc")
        .resized(
            SizeConstraint::Fixed(layout.doc.x),
            SizeConstraint::Fixed(layout.doc.y),
        )
}

pub fn post_layout(siv: &mut Cursive) {
    let cur_id = CUR_DOC.read().unwrap().id.clone();
    if cur_id.len() == 0 {
        show_doc(siv, "index", true, false);
    } else {
        show_doc(siv, &cur_id, false, false);
    }
    let _ = siv.focus_name("doc");
}
