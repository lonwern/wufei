use colored::*;
use rand::seq::SliceRandom;
use std::fs::File;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::str;

use std::{thread, time};
use structopt::StructOpt;
use tokio_threadpool::{blocking, ThreadPool};

use futures::future::{lazy, poll_fn};
use futures::Future;

use kube_async::{
    api::v1Event,
    api::{Api, Informer, ListParams, WatchEvent},
    client::APIClient,
    config,
};
use new_futures::stream::StreamExt;
use once_cell::sync::OnceCell;

/// Static string to hold values we want to use to differentiate between pod logs.  These colors
/// are mapped from the colored cargo crate
static COLOR_VEC: &'static [&str] = &[
    "green",
    "red",
    "yellow",
    "blue",
    "cyan",
    "magenta",
    "white",
    "bright black",
    "bright red",
    "bright green",
    "bright yellow",
    "bright blue",
    "bright magenta",
    "bright cyan",
];

#[derive(Clone, Debug, StructOpt)]
#[structopt(
    name = "Wufei",
    about = "Tail ALL your kubernetes logs at once, or record them to files",
    author = "Eric McBride <ericmcbridedeveloper@gmail.com> github.com/ericmcbride"
)]
pub struct LogRecorderConfig {
    /// Namespace for logs
    #[structopt(short, long, default_value = "kube-system")]
    pub namespace: String,

    /// The kube config for accessing your cluster.
    #[structopt(short, long = "kubeconfig", default_value = "")]
    kube_config: String,

    /// Record the logs to a file. Note: Logs will not appear in stdout.
    #[structopt(short, long)]
    file: bool,

    /// Outfile of where the logs are being recorded
    #[structopt(short, long, default_value = "/tmp/wufei/")]
    outfile: String,

    /// Pods for the logs will appear in color in your terminal
    #[structopt(long)]
    color: bool,

    /// Runs an informer, that will add new pods to the tailed logs
    #[structopt(long)]
    pub update: bool,
}


pub static CONFIG: OnceCell<LogRecorderConfig> = OnceCell::new();

impl LogRecorderConfig {
    pub fn global() -> &'static LogRecorderConfig {
        CONFIG.get().expect("Config is not initialized")
    }
}

/// Pod infromation
#[derive(Clone, Debug, Default)]
pub struct PodInfo {
    name: String,
    container: String,
    parent: String,
    file_name: String,
}

/// Cli options for wufei
pub fn generate_config() -> LogRecorderConfig {
    let opt = LogRecorderConfig::from_args();
    opt
}

/// Entrypoint for the tailing of the logs
pub async fn run_logs() -> Result<(), Box<dyn ::std::error::Error>> {
    let pod_vec = get_all_pod_info().await?;
    run_cmd(pod_vec).await?;
    Ok(())
}

///  Kicks off the concurrent logging
async fn run_cmd(
    pods: Vec<PodInfo>,
) -> Result<(), Box<dyn ::std::error::Error>> {
    let mut children = vec![];
    if LogRecorderConfig::global().file {
        tokio::fs::create_dir_all(&LogRecorderConfig::global().outfile).await?;
    }

    // visit once cell to make this a singleton https://github.com/matklad/once_cell
    let pool = ThreadPool::new();
    
    println!("Beginning to tail logs, press <ctrl> + c to kill wufei...");
    for pod in pods {
        // In this chunk of code we are using a tokio threadpool.  The threadpool runs a task,
        // which can easily be compared to a green thread or a GO routine.  We do not have a
        // complicated requirement here, so we use just use futures built in poll_fn which is a
        // stream wrapper function that returns a poll.  This satisifies the pool.spawn function
        children.push(pool.spawn(lazy(move || {
            poll_fn(move || {
                blocking(|| run_individual(&pod))
                    .map_err(|_| panic!("the threadpool shutdown"))
            })
        })));
    }
    pool.shutdown_on_idle().wait().unwrap();
    Ok(())
}

/// Each thread runs this function.   It gathers the individual logs at a thread level (pod
/// level in this case).  It does all the filtering of the cli args, spins off a background
/// process to tail the logs, and buffers the terminal output, allowing the each thread to print
/// out to stdout in a non-blocking fashion.  If an error happens, instead of handling using
/// channels, we are just writing the stderr into the output file if the flag exists.  If not the
/// thread (or pod) buffer will not be outputted to stdout
fn run_individual(pod_info: &PodInfo) {
    let mut kube_cmd = Command::new("kubectl");
    if LogRecorderConfig::global().kube_config.len() != 0 {
        kube_cmd.arg("--kubeconfig");
        kube_cmd.arg(&LogRecorderConfig::global().kube_config);
    }

    kube_cmd.arg("logs");
    kube_cmd.arg("-f");
    kube_cmd.arg(&pod_info.parent);
    kube_cmd.arg(&pod_info.container);
    kube_cmd.arg("-n");
    kube_cmd.arg(&LogRecorderConfig::global().namespace);
    kube_cmd.stdout(Stdio::piped());

    let output = kube_cmd
        .spawn()
        .unwrap()
        .stdout
        .ok_or_else(|| "Unable to follow kube logs")
        .unwrap();

    let reader = BufReader::new(output);
    let mut log_prefix = "[".to_owned() + &pod_info.parent + "][" + &pod_info.container + "]";

    if  LogRecorderConfig::global().color {
        let color = COLOR_VEC.choose(&mut rand::thread_rng()); // get random color
        let str_color = color.unwrap().to_string(); // unwrap random
        log_prefix = log_prefix.color(str_color).to_string();
    }

    if LogRecorderConfig::global().file {
        let mut out_file = File::create(&pod_info.file_name).unwrap();
        reader
            .lines()
            .filter_map(|line| line.ok())
            .for_each(|line| {
                let new_line = format!("{}\n", line);
                out_file.write(&new_line.as_bytes()).unwrap();
            });
    } else {
        reader
            .lines()
            .filter_map(|line| line.ok())
            .for_each(|line| {
                let log_msg = format!("{}: {}\n", &log_prefix, line);
                let _ = std::io::stdout().write(log_msg.as_bytes());
            });
    }
}

/// A thin wrapper around the run_individual function. This allows adding of new pods to the
/// threadpool.  Hopefully there will be a cleaner way to do this in the future.  Feels like a hack right now
async fn run_individual_async(pod_info: PodInfo) {
    thread::spawn(move || {
        println!(
            "Informer found new pod: {:?} with container: {:?}, starting to tail the logs",
            pod_info.name, pod_info.container,
        );
        run_individual(&pod_info)
    });
}

/// Gather all information about the pods currently deployed in the users kubernetes cluster
async fn get_all_pod_info(
) -> Result<(Vec<PodInfo>), Box<dyn ::std::error::Error>> {
    println!("Getting all pods in namespace {}...", LogRecorderConfig::global().namespace); 
    let client = get_kube_client().await;
    let pods = Api::v1Pod(client.clone()).within(&LogRecorderConfig::global().namespace);
    let mut pod_info_vec: Vec<PodInfo> = vec![];

    for p in pods.list(&ListParams::default()).await? {
        for c in p.spec.containers {
            let container = c.name;
            let pod_name = p.metadata.name.to_string();
            let file_name = 
                LogRecorderConfig::global().outfile.to_string() + &pod_name + "-" + &container + ".txt";

            let pod_info = PodInfo {
                name: pod_name.clone(),
                container: container,
                parent: pod_name.clone(),
                file_name: file_name,
            };
            pod_info_vec.push(pod_info);
        }
    }

    Ok(pod_info_vec)
}

/// An informer that will update the main thread pool if a new pod is spun up.
pub async fn pod_informer() -> Result<(), Box<dyn ::std::error::Error>> {
    let client = get_kube_client().await;
    let events = Api::v1Event(client);
    let ei = Informer::new(events).init().await?;
    loop {
        let mut events = ei.poll().await.unwrap().boxed();

        while let Some(event) = events.next().await {
            let event = event?;
            handle_events(event).await?;
        }
    }
}

/// Watches for an event.  If there is a new added event, we check if its a created pod type.  If
/// it is we see if the pod exists in the clusters namespace, and if it does exist, we make sure
/// the pod is healthy.  If the pod is healthy, we had it to the threadpool and begin tailing the
/// containers in the pod
async fn handle_events(ev: WatchEvent<v1Event>) -> Result<(), Box<dyn ::std::error::Error>> {
    match ev {
        WatchEvent::Added(o) => {
            if o.message.contains("Created pod") {
                println!("{}, checking to see if this event effects wufei", o.message);

                let pod_message: Vec<&str> = o.message.split(":").collect();
                let pod_str = pod_message[1].trim();

                let pods = get_all_pod_info().await?;
                for p in pods {
                    if pod_str == p.name {
                        loop {
                            let healthy = check_status(&p.name).await?;
                            if healthy {
                                break;
                            }
                            let five_secs = time::Duration::from_secs(5);
                            thread::sleep(five_secs);
                        }
                        run_individual_async(p).await;
                    }
                }
            }
        }
        WatchEvent::Modified(_) => {}
        WatchEvent::Deleted(_) => {}
        WatchEvent::Error(_) => {}
    }
    Ok(())
}

/// Checks to see if the newly created pod is healthy, if the pod is healthy, then it is ready to
/// be added to the logging threadpool
async fn check_status(pod: &str) -> Result<bool, Box<dyn ::std::error::Error>> {
    let client = get_kube_client().await;
    let pods = Api::v1Pod(client).within(&LogRecorderConfig::global().namespace);
    let pod_obj = pods.get(pod).await?;
    let status = pod_obj.status.unwrap().phase.unwrap();

    if status != "Running" {
        return Ok(false);
    }
    Ok(true)
}

// do something for this.  lazy_static doesnt support await syntax, and singleton maybe out of
// scope.  Some overhead to this.
async fn get_kube_client() -> APIClient {
    let config = config::load_kube_config().await.unwrap();
    APIClient::new(config)
}
