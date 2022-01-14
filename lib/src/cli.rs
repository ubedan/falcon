// Copyright 2021 Oxide Computer Company

use std::fs;
use std::{
    net::{IpAddr, SocketAddr, Ipv4Addr},
    os::unix::prelude::AsRawFd,
    io::{stdout, Write},
};
use std::process::Command;

use anyhow::{anyhow, Context};
use futures::{SinkExt, StreamExt};
use propolis_client::{
    api::InstanceStateRequested,
    Client,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use slog::{warn, o, Drain, Level, Logger};
use colored::*;
use tabwriter::TabWriter;
use ron::de::{from_str};

use clap::{AppSettings, Parser};

use crate::{error::Error, Runner, Deployment};

pub enum RunMode {
    Unspec,
    Launch,
    Destroy,
}

#[derive(Parser)]
#[clap(
    version = "0.1",
    author = "Ryan Goodfellow <ryan.goodfellow@oxide.computer>"
)]
#[clap(setting = AppSettings::InferSubcommands)]
struct Opts {
    #[clap(short, long, parse(from_occurrences))]
    verbose: i32,

    #[clap(subcommand)]
    subcmd: SubCommand,
}


#[derive(Parser)]
enum SubCommand {
    #[clap(about = "launch topology")]
    Launch(CmdLaunch),
    #[clap(about = "destroy topology")]
    Destroy(CmdDestroy),
    #[clap(about = "get a serial console session for the specified vm")]
    Serial(CmdSerial),
    #[clap(about = "display topology information")]
    Info(CmdInfo),
    #[clap(about = "reboot a vm")]
    Reboot(CmdReboot),
    #[clap(about = "stop a vm's hypervisor")]
    Hyperstop(CmdHyperstop),
    #[clap(about = "start a vm's hypervisor")]
    Hyperstart(CmdHyperstart),
    #[clap(about = "create a topology's network")]
    Netcreate(CmdNetCreate),
    #[clap(about = "destroy a topology's network")]
    Netdestroy(CmdNetDestroy),
    #[clap(about = "snapshot a node")]
    Snapshot(CmdSnapshot),
}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdLaunch {

    /// The propolis-server binary to use
    #[clap(short, long)]
    propolis: Option<String>

}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdDestroy {}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdSerial {

    /// Name of the VM to establish a serial connection to
    vm_name: String,

}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdReboot {

    /// Name of the VM to reboot
    vm_name: String,

}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdHyperstop {

    /// Name of the vm to stop
    vm_name: Option<String>,

    /// Stop all vms in the topology
    #[clap(short, long)]
    all: bool,
}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdHyperstart {

    /// The propolis-server binary to use
    #[clap(short, long)]
    propolis: Option<String>,

    /// Name of the vm to start
    vm_name: Option<String>,

    /// Start all vms in the topology
    #[clap(short, long)]
    all: bool,
}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdNetCreate { }

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdNetDestroy { }

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdSnapshot { 

    /// Name of the VM to snaphost
    vm_name: String,

    /// What to name the new snapshot
    snapshot_name: String,
}

#[derive(Parser)]
#[clap(setting = AppSettings::InferSubcommands)]
struct CmdInfo {}

/// Entry point for a command line application. Will parse command line
/// arguments and take actions accordingly.
///
/// # Examples
/// ```no_run
/// use libfalcon::{cli::run, Runner};
/// fn main() {
///     let mut r = Runner::new("duo");
///
///     // nodes
///     let violin = r.zone("violin");
///     let piano = r.zone("piano");
///
///     // links
///     r.link(violin, piano);
///
///     run(&mut r);
/// }
/// ```
pub async fn run(r: &mut Runner) -> Result<RunMode, Error> {
    r.persistent = true;

    let opts: Opts = Opts::parse();
    match opts.subcmd {
        SubCommand::Launch(l) => {
            match l.propolis {
                Some(path) => r.propolis_binary = path,
                None => {}
            };
            launch(r).await;
            Ok(RunMode::Launch)
        },
        SubCommand::Destroy(_) => {
            destroy(r);
            Ok(RunMode::Destroy)
        },
        SubCommand::Serial(ref c) => {
            console(&c.vm_name).await?;
            Ok(RunMode::Unspec)
        },
        SubCommand::Info(_) => {
            info(r)?;
            Ok(RunMode::Unspec)
        }
        SubCommand::Reboot(ref c) => {
            reboot(&c.vm_name).await?;
            Ok(RunMode::Unspec)
        },
        SubCommand::Hyperstop(ref c) => {
            if c.all {
                for x in &r.deployment.nodes {
                    hyperstop(&x.name).await?;
                }
            } else {
                match c.vm_name {
                    None => return Err(Error::Cli(
                            "vm name required unless --all flag is used".into())),
                    Some(ref n) => hyperstop(n).await?,
                }
            }
            Ok(RunMode::Unspec)
        },
        SubCommand::Hyperstart(ref c) => {
            let propolis_binary = match c.propolis {
                Some(ref path) => path.clone(),
                None => "propolis-server".into(),
            };
            if c.all {
                for x in &r.deployment.nodes {
                    hyperstart(&x.name, propolis_binary.clone()).await?;
                }
            } else {
                match c.vm_name {
                    None => return Err(Error::Cli(
                            "vm name required unless --all flag is used".into())),
                    Some(ref n) => hyperstart(n, propolis_binary).await?,
                }
            }
            Ok(RunMode::Unspec)
        },
        SubCommand::Netcreate(_) => {
            netcreate(r).await;
            Ok(RunMode::Unspec)
        }
        SubCommand::Netdestroy(_) => {
            netdestroy(r);
            Ok(RunMode::Unspec)
        }
        SubCommand::Snapshot(s) => {
            snapshot(s)?;
            Ok(RunMode::Unspec)
        }
    }

}

fn info(r: &Runner) -> anyhow::Result<()> {

    let mut tw = TabWriter::new(stdout());

    println!("{} {}",
        "name:".dimmed(),
        r.deployment.name,
    );

    println!("{}", "Nodes".bright_black());
    write!(
        &mut tw,
        "{}\t{}\t{}\t{}\t{}\n",
        "Name".dimmed(),
        "Image".dimmed(),
        "Radix".dimmed(),
        "Mounts".dimmed(),
        "UUID".dimmed(),
    )?;
    write!(
        &mut tw,
        "{}\t{}\t{}\t{}\t{}\n",
        "----".bright_black(),
        "-----".bright_black(),
        "-----".bright_black(),
        "------".bright_black(),
        "----".bright_black(),
    )?;
    for x in &r.deployment.nodes {
        let mount = {
            if x.mounts.len() > 0 {
                format!("{} -> {}",
                    x.mounts[0].source,
                    x.mounts[0].destination,
                )
            } else {
                "".into()
            }
        };
        write!(
            &mut tw,
            "{}\t{}\t{}\t{}\t{}\n",
            x.name,
            x.image,
            x.radix,
            mount,
            x.id,
        )?;
        if x.mounts.len() > 1 {
            for m in &x.mounts[1..] {
                let mount = format!("{} -> {}",
                    m.source,
                    m.destination,
                );
                write!(
                    &mut tw,
                    "{}\t{}\t{}\t{}\t{}\n",
                    "",
                    "",
                    "",
                    mount,
                    "",
                )?;
            }
        }
    }
    tw.flush()?;

    Ok(())

}

async fn launch(r: &Runner) {
    match r.launch().await {
        Err(e) => println!("{}", e),
        Ok(()) => {}
    }
}

async fn netcreate(r: &Runner) {
    match r.net_launch().await {
        Err(e) => println!("{}", e),
        Ok(()) => {}
    }
}

fn netdestroy(r: &Runner) {
    match r.net_destroy() {
        Err(e) => println!("{}", e),
        Ok(()) => {}
    }
}

fn snapshot(cmd: CmdSnapshot) -> Result<(), Error> {

    // read topology
    let topo_ron = fs::read_to_string(".falcon/topology.ron")?;
    let d: Deployment = from_str(&topo_ron)?; 

    // get node from topology
    let mut node = None;
    for n in &d.nodes {
        if n.name == cmd.vm_name {
            node = Some(n);
        }
    }

    let node = match node {
        None => {
            return Err(Error::NotFound(cmd.vm_name.into()))
        }
        Some(node) => node
    };

    let source = format!("rpool/falcon/topo/{}/{}",
        d.name,
        node.name
    );
    let source_snapshot = format!("{}@base", source);

    let dest = format!("rpool/falcon/img/{}",
        cmd.snapshot_name,
    );
    let dest_snapshot = format!("{}@base", source);

    // first take a snapshot of the node clone
    let out = Command::new("zfs")
        .args(&[
            "snapshot",
            source_snapshot.as_ref(),
        ]).output()?;
    if !out.status.success() {
        return Err(Error::Zfs(String::from_utf8(out.stderr)?))
    }

    // next clone the source snapshot to a new base image
    let out = Command::new("zfs")
        .args(&[
            "clone",
            source_snapshot.as_ref(),
            dest.as_ref(),
        ]).output()?;

    if !out.status.success() {
        return Err(Error::Zfs(String::from_utf8(out.stderr)?))
    }

    // promote the base image to uncouple from source snapshot
    let out = Command::new("zfs")
        .args(&[
            "promote",
            dest.as_ref(),
        ]).output()?;
    if !out.status.success() {
        return Err(Error::Zfs(String::from_utf8(out.stderr)?))
    }

    // finally create base snapshot for new image
    let out = Command::new("zfs")
        .args(&[
            "snapshot",
            dest_snapshot.as_ref(),
        ]).output()?;
    if !out.status.success() {
        return Err(Error::Zfs(String::from_utf8(out.stderr)?))
    }


    Ok(())
}

fn destroy(r: &Runner) {
    match r.destroy() {
        Err(e) => println!("{}", e),
        Ok(()) => {}
    }
}

async fn console(name: &str) -> Result<(), Error> {

    let port: u16 = fs::read_to_string(format!(".falcon/{}.port", name))?
        .trim_end()
        .parse()?;

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127,0,0,1)), port);
    let log = create_logger();
    let client = Client::new(addr.clone(), log.new(o!()));

    serial(
        &client,
        addr.clone(),
        name.into(),
    ).await?;

    Ok(())

}

// TODO copy pasta from propolis/cli/src/main.rs
async fn serial(
    client: &Client,
    addr: SocketAddr,
    name: String,
) -> anyhow::Result<()> {
    // Grab the Instance UUID
    let id = client
        .instance_get_uuid(&name)
        .await
        .with_context(|| anyhow!("failed to get instance UUID"))?;

    let path = format!("ws://{}/instances/{}/serial", addr, id);
    let (mut ws, _) = tokio_tungstenite::connect_async(path)
        .await
        .with_context(|| anyhow!("failed to create serial websocket stream"))?;

    let _raw_guard = RawTermiosGuard::stdio_guard()
        .with_context(|| anyhow!("failed to set raw mode"))?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            c = stdin.read_u8() => {
                match c? {
                    // Exit on Ctrl-Q
                    b'\x11' => break,
                    c => ws.send(Message::binary(vec![c])).await?,
                }
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(input))) => {
                        stdout.write_all(&input).await?;
                        stdout.flush().await?;
                    }
                    Some(Ok(Message::Close(..))) | None => break,
                    _ => continue,
                }
            }
        }
    }

    Ok(())
}

/// Guard object that will set the terminal to raw mode and restore it
/// to its previous state when it's dropped
struct RawTermiosGuard(libc::c_int, libc::termios);

impl RawTermiosGuard {
    fn stdio_guard() -> Result<RawTermiosGuard, std::io::Error> {
        let fd = std::io::stdout().as_raw_fd();
        let termios = unsafe {
            let mut curr_termios = std::mem::zeroed();
            let r = libc::tcgetattr(fd, &mut curr_termios);
            if r == -1 {
                return Err(std::io::Error::last_os_error());
            }
            curr_termios
        };
        let guard = RawTermiosGuard(fd, termios.clone());
        unsafe {
            let mut raw_termios = termios;
            libc::cfmakeraw(&mut raw_termios);
            let r = libc::tcsetattr(fd, libc::TCSAFLUSH, &raw_termios);
            if r == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(guard)
    }
}
impl Drop for RawTermiosGuard {
    fn drop(&mut self) {
        let r = unsafe { libc::tcsetattr(self.0, libc::TCSADRAIN, &self.1) };
        if r == -1 {
            Err::<(), _>(std::io::Error::last_os_error()).unwrap();
        }
    }
}

/// Create a top-level logger that outputs to stderr
fn create_logger() -> Logger {
    let decorator = slog_term::TermDecorator::new().stderr().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let level =  Level::Debug;
    let drain = slog::LevelFilter(drain, level).fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let logger = Logger::root(drain, o!());
    logger
}

async fn reboot(name: &str) -> Result<(), Error> {

    let port: u16 = fs::read_to_string(format!(".falcon/{}.port", name))?
        .trim_end()
        .parse()?;

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127,0,0,1)), port);
    let log = create_logger();
    let client = Client::new(addr.clone(), log.new(o!()));

    // Grab the Instance UUID
    let id = client
        .instance_get_uuid(&name)
        .await
        .with_context(|| anyhow!("failed to get instance UUID"))?;

    // reboot
    client
        .instance_state_put(id, InstanceStateRequested::Reboot)
        .await
        .with_context(|| anyhow!("failed to reboot machine"))?;

    Ok(())

}

async fn hyperstop(name: &str) -> Result<(), Error> {

    let log = create_logger();

    let pidfile = format!(".falcon/{}.pid", name);
    
    // read pid
    match fs::read_to_string(&pidfile) {
        Ok(pid) => {
            match pid.trim_end().parse() {
                Ok(pid) => {
                    unsafe { libc::kill(pid, libc::SIGKILL); }
                    fs::remove_file(pidfile)?;
                }
                Err(e) => warn!(log, "could not parse pidfile for {}: {}", name, e),
            }
        }
        Err(e) => {
            warn!(log, "could not get pidfile for {}: {}", name, e);
        }
    };


    // get instance uuid
    let uuid = match fs::read_to_string(format!(".falcon/{}.uuid", name)) {
        Ok(u) => u,
        Err(e) => {
            warn!(log, "get propolis uuid for {}: {}", name, e);
            return Ok(());
        }
    };

    // destroy bhyve vm
    let vm_arg = format!("--vm={}", uuid);
    match Command::new("bhyvectl").args(&["--destroy", vm_arg.as_ref()]).output() {
        Ok(_) => {}
        Err(e) => {
            warn!(log, "delete bhyve vm for {}: {}", name, e);
            return Ok(());
        }
    }

    Ok(())
}

async fn hyperstart(name: &str, propolis_binary: String) -> Result<(), Error> {

    // read topology
    let topo_ron = fs::read_to_string(".falcon/topology.ron")?;
    let d: Deployment = from_str(&topo_ron)?; 

    let mut node = None;
    for n in &d.nodes {
        if n.name == name {
            node = Some(n);
        }
    }

    let node = match node {
        None => {
            return Err(Error::NotFound(name.into()))
        }
        Some(node) => node
    };

    let port: u32 = fs::read_to_string(format!(".falcon/{}.port", name))?
        .trim_end()
        .parse()?;
    let id: uuid::Uuid = fs::read_to_string(format!(".falcon/{}.uuid", name))?
        .trim_end()
        .parse()?;
    let log = create_logger();

    crate::launch_vm(&log, &propolis_binary, port, &id, node).await?;

    Ok(())
}
