use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use structopt::StructOpt;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

mod config;
mod hub_responses;

use config::*;
use hub_responses::*;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Arguments = StructOpt::from_args();
    println!("{:#?}", args);

    let config = read_config(args.config).await?;
    let file_manager =
        FileManager::new(args.output.unwrap_or_else(|| "test".into()), args.verbose).await?;

    // let manager = IoTHubDeviceManager::new(&config, &file_manager);
    // if args.delete {
    //     manager.delete_devices().await?;
    //     return Ok(());
    // }

    // let devices = manager.create_devices().await?;
    // let devices: HashMap<String, CreateResponse> = devices
    //     .into_iter()
    //     .map(|d| (d.device_id.clone(), d))
    //     .collect();

    // Windows only, run
    //$Env:OPENSSL_CONF="C:\Users\Lee\source\GnuWin32\share\openssl.cnf"
    // #[cfg(any(windows))]
    // let openssl = Some(Path::new(r"C:\Users\Lee\source\GnuWin32\bin\openssl.exe"));
    // #[cfg(any(unix))]
    // let openssl = None;

    let cert_manager = CertManager::new(&config, &file_manager, args.openssl_path.as_deref());

    cert_manager.make_root_cert().await?;
    cert_manager.make_all_device_certs().await?;

    // visualize(&config.root_device)?;
    Ok(())
}

#[derive(StructOpt, Debug)]
struct Arguments {
    /// Verbose: gives more detailed output
    #[structopt(short, long)]
    verbose: bool,

    /// Delete: deletes devices in hub instead fo creating them
    #[structopt(short, long)]
    delete: bool,

    /// Output: path to create directory at. Default: `./nested`
    #[structopt(short, long)]
    output: Option<PathBuf>,

    /// Config: path to config file. Default: `./nested_config.yaml`
    #[structopt(short, long)]
    config: Option<PathBuf>,

    /// Path to openssl executable. Only needed if `openssl` is not in PATH.
    #[structopt(long)]
    openssl_path: Option<PathBuf>,
}

async fn read_config(file_path: Option<PathBuf>) -> Result<Config> {
    let file_path = file_path.unwrap_or_else(|| "./templates/test1.yaml".into());

    println!("Reading {:?}", file_path);
    let is_toml = file_path.to_str().unwrap().ends_with(".toml");

    let data = fs::read(file_path).await.context("Error reading file")?;

    let config = if is_toml {
        toml::from_slice(&data).context("Error parsing data")?
    } else {
        serde_yaml::from_slice(&data).context("Error parsing data")?
    };

    Ok(config)
}

fn get_command() -> Command {
    #[cfg(any(unix))]
    {
        Command::new("sh")
    }

    #[cfg(any(windows))]
    {
        Command::new("powershell.exe")
    }
}

fn flatten_devices(device: &DeviceConfig) -> Vec<&str> {
    let mut result: Vec<&str> = vec![&device.device_id];
    for child in &device.children {
        result.append(&mut flatten_devices(&child));
    }

    result
}

struct IoTHubDeviceManager<'a> {
    config: &'a Config,
    file_manager: &'a FileManager,
}

impl<'a> IoTHubDeviceManager<'a> {
    pub fn new(config: &'a Config, file_manager: &'a FileManager) -> Self {
        Self {
            config,
            file_manager,
        }
    }

    pub async fn create_devices(&self) -> Result<Vec<CreateResponse>> {
        // Create devices
        let devices_to_create = flatten_devices(&self.config.root_device);
        self.file_manager
            .print(&format!(
                "Creating {} devices in hub {}",
                devices_to_create.len(),
                self.config.iothub.iot_hub_name
            ))
            .await?;

        let futures = devices_to_create
            .iter()
            .map(|d| self.create_device_identity(d));

        let created_devices = futures::future::join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<CreateResponse>>>()?;
        // Add parent-child relationships
        let relationships_to_add = Self::get_relationships(&self.config.root_device);
        self.file_manager
            .print(&format!(
                "Created all devices. Adding {} parent-child relationships.",
                relationships_to_add.len()
            ))
            .await?;

        let futures = relationships_to_add
            .iter()
            .map(|(parent, child)| self.create_parent_child_relationship(parent, child));

        futures::future::join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<()>>>()?;

        Ok(created_devices)
    }

    pub async fn delete_devices(&self) -> Result<()> {
        let devices_to_delete = flatten_devices(&self.config.root_device);
        self.file_manager
            .print(&format!(
                "Deleting {} devices from hub {}",
                devices_to_delete.len(),
                self.config.iothub.iot_hub_name
            ))
            .await?;

        let futures = devices_to_delete
            .iter()
            .map(|d| self.delete_device_identity(d));

        let num_successes = futures::future::join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<bool>>>()?
            .into_iter()
            .filter(|s| *s)
            .count();

        if num_successes == devices_to_delete.len() {
            self.file_manager.print("Deleted all devices.").await?;
        } else {
            self.file_manager
                .print(&format!(
                "Successfully deleted {} devices, {} failed. For more information use the -v flag.",
                num_successes,
                num_successes - devices_to_delete.len(),
            ))
                .await?;
        }

        Ok(())
    }

    fn get_relationships(device: &DeviceConfig) -> Vec<(&str, &str)> {
        let mut result: Vec<(&str, &str)> = Vec::new();
        for child in &device.children {
            result.push((&device.device_id, &child.device_id));
            result.append(&mut Self::get_relationships(&child));
        }

        result
    }

    async fn create_device_identity(&self, device_id: &str) -> Result<CreateResponse> {
        self.file_manager
            .print_verbose(format!(
                "Creating device {} on hub {}",
                device_id, self.config.iothub.iot_hub_name
            ))
            .await?;

        let command = get_command()
            .arg("az iot hub device-identity create")
            .args(&["--device-id", device_id])
            .args(&["--hub-name", &self.config.iothub.iot_hub_name])
            .arg("--edge-enabled")
            .output()
            .await?;

        if command.status.success() {
            self.file_manager
                .print_verbose(format!("Successfully created {}", device_id))
                .await?;

            let created_device: CreateResponse = serde_json::from_slice(&command.stdout)?;
            Ok(created_device)
        } else {
            let error = format!(
                "Failed to create {}:\n{}\n{}\n",
                device_id,
                String::from_utf8_lossy(&command.stdout),
                String::from_utf8_lossy(&command.stderr)
            );
            self.file_manager.print_verbose(&error).await?;

            Err(anyhow::Error::msg(error))
        }
    }

    async fn create_parent_child_relationship(&self, parent: &str, child: &str) -> Result<()> {
        self.file_manager
            .print_verbose(format!("Adding {} as child of parent {}.", child, parent,))
            .await?;

        let command = get_command()
            .arg("az iot hub device-identity parent set")
            .args(&["--device-id", child])
            .args(&["--parent-device-id", parent])
            .args(&["--hub-name", &self.config.iothub.iot_hub_name])
            .output()
            .await?;

        if command.status.success() {
            self.file_manager
                .print_verbose(format!(
                    "Successfully added {} as child of parent {}.",
                    child, parent,
                ))
                .await?;

            Ok(())
        } else {
            let error = format!(
                "Failed to add {} as child of parent {}:\n{}\n{}\n",
                child,
                parent,
                String::from_utf8_lossy(&command.stdout),
                String::from_utf8_lossy(&command.stderr)
            );
            self.file_manager.print_verbose(&error).await?;

            Err(anyhow::Error::msg(error))
        }
    }

    async fn delete_device_identity(&self, device_id: &str) -> Result<bool> {
        self.file_manager
            .print_verbose(format!(
                "Deleting device {} on hub {}",
                device_id, self.config.iothub.iot_hub_name
            ))
            .await?;

        let command = get_command()
            .arg("az iot hub device-identity delete")
            .args(&["--device-id", device_id])
            .args(&["--hub-name", &self.config.iothub.iot_hub_name])
            .output()
            .await?;

        if command.status.success()
            || String::from_utf8_lossy(&command.stderr).contains("ErrorCode:DeviceNotFound;")
        {
            self.file_manager
                .print_verbose(format!("Successfully deleted {}", device_id))
                .await?;
            Ok(true)
        } else {
            self.file_manager
                .print_verbose(format!(
                    "Failed to delete {}:\n{}\n{}\n",
                    device_id,
                    String::from_utf8_lossy(&command.stdout),
                    String::from_utf8_lossy(&command.stderr)
                ))
                .await?;

            Ok(false)
        }
    }
}

struct CertManager<'a> {
    config: &'a Config,
    file_manager: &'a FileManager,
    openssl_path: Option<&'a Path>,
}

impl<'a> CertManager<'a> {
    pub fn new(
        config: &'a Config,
        file_manager: &'a FileManager,
        openssl_path: Option<&'a Path>,
    ) -> Self {
        Self {
            config,
            file_manager,
            openssl_path,
        }
    }

    async fn make_all_device_certs(&self) -> Result<()> {
        let certs_to_make = flatten_devices(&self.config.root_device);
        self.file_manager
            .print(&format!(
                "Creating certs for {} devices",
                certs_to_make.len(),
            ))
            .await?;

        let futures = certs_to_make.iter().map(|d| self.make_device_cert(d));

        let num_successes = futures::future::join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<()>>>()?;

        self.file_manager.print("Created all device certs.").await?;

        Ok(())
    }

    async fn make_root_cert(&self) -> Result<()> {
        self.file_manager.print("Making Root CA.").await?;
        let cert_folder = self.file_manager.get_folder("certs").await?;
        let command = self
            .openssl_path
            .map_or_else(|| Command::new("openssl"), Command::new)
            .arg("req")
            .args(&[
                "-x509", "-new", "-newkey", "rsa:4096", "-days", "365", "-nodes",
            ])
            .args(&[
                OsStr::new("-keyout"),
                cert_folder.join("root.key.pem").as_os_str(),
            ])
            .args(&[OsStr::new("-out"), cert_folder.join("root.pem").as_os_str()])
            .args(&["-subj", "/CN=Azure_IoT_Nested_Cert"])
            .output()
            .await?;

        self.file_manager
            .print_verbose(format!(
                "{}{}",
                String::from_utf8_lossy(&command.stdout),
                String::from_utf8_lossy(&command.stderr)
            ))
            .await?;

        self.file_manager
            .print(format!(
                "Successfully made Root CA {:?}.",
                cert_folder.join("root.pem")
            ))
            .await?;

        Ok(())
    }

    async fn make_device_cert(&self, device_id: &str) -> Result<()> {
        self.file_manager
            .print_verbose(format!("Making device CA for {}.", device_id))
            .await?;

        // TODO: make cert correctly
        let device_folder = self.file_manager.get_folder(device_id).await?;
        let command = self
            .openssl_path
            .map_or_else(|| Command::new("openssl"), Command::new)
            .arg("req")
            .args(&[
                "-x509", "-new", "-newkey", "rsa:4096", "-days", "365", "-nodes",
            ])
            .args(&[
                OsStr::new("-keyout"),
                device_folder.join("key.pem").as_os_str(),
            ])
            .args(&[
                OsStr::new("-out"),
                device_folder.join("cert.pem").as_os_str(),
            ])
            .args(&["-subj", "/CN=Azure_IoT_Nested_Cert"])
            // .spawn()?;
            .output()
            .await?;

        self.file_manager
            .print_verbose(format!(
                "{}{}",
                String::from_utf8_lossy(&command.stdout),
                String::from_utf8_lossy(&command.stderr)
            ))
            .await?;

        self.file_manager
            .print_verbose(format!(
                "Successfully made CA {:?}.",
                device_folder.join("cert.pem")
            ))
            .await?;

        Ok(())
    }
}
use std::path::{Path, PathBuf};
struct FileManager {
    base_path: PathBuf,
    log_file: Arc<Mutex<fs::File>>,
    verbose: bool,
}

impl FileManager {
    async fn new<P>(base_path: P, verbose: bool) -> Result<Self>
    where
        P: Into<PathBuf>,
    {
        let base_path: PathBuf = base_path.into();
        fs::create_dir_all(&base_path).await?;

        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let log_file = base_path.join(format!("log_{}.txt", time));
        let log_file = fs::File::create(log_file).await?;
        let log_file = Arc::new(Mutex::new(log_file));
        Ok(Self {
            base_path,
            log_file,
            verbose,
        })
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    async fn get_folder(&self, path: &str) -> Result<PathBuf> {
        let mut folder = self.base_path.clone();
        folder.push(path);

        fs::create_dir_all(&folder).await?;

        Ok(folder)
    }

    async fn print<S>(&self, text: S) -> Result<()>
    where
        S: AsRef<str>,
    {
        println!("{}", text.as_ref());

        self.write_log(&format!("{}\n", text.as_ref())).await?;
        Ok(())
    }

    async fn print_verbose<S>(&self, text: S) -> Result<()>
    where
        S: AsRef<str>,
    {
        if self.verbose {
            println!("{}", text.as_ref());
        }

        self.write_log(&format!("{}\n", text.as_ref())).await?;
        Ok(())
    }

    async fn write_log(&self, text: &str) -> Result<()> {
        let log_file = self.log_file.clone();
        let mut log_file = log_file.lock().await;
        log_file.write_all(text.as_bytes()).await?;
        Ok(())
    }
}

// use id_tree::InsertBehavior::{AsRoot, UnderNode};
// use id_tree::{Node, NodeId, Tree, TreeBuilder};
// use id_tree_layout::{Layouter, Visualize};

// struct NodeData(String);

// fn visualize(root: &DeviceConfig) -> Result<()> {
//     let mut tree: Tree<NodeData> = TreeBuilder::new().build();

//     let root_id: NodeId = tree.insert(Node::new(NodeData(root.device_id.clone())), AsRoot)?;
//     add_children(&root.children, &root_id, &mut tree)?;

//     fs::create_dir_all("test")?;
//     Layouter::new(&tree)
//         .with_file_path(std::path::Path::new("test/visualization.svg"))
//         .write()
//         .context("Cannot write visualization file.")?;

//     Ok(())
// }

// fn add_children(
//     children: &[DeviceConfig],
//     parent: &NodeId,
//     tree: &mut Tree<NodeData>,
// ) -> Result<()> {
//     for child in children {
//         let new_node: NodeId = tree.insert(
//             Node::new(NodeData(child.device_id.clone())),
//             UnderNode(parent),
//         )?;
//         add_children(&child.children, &new_node, tree)?;
//     }

//     Ok(())
// }

// impl Visualize for NodeData {
//     fn visualize(&self) -> std::string::String {
//         // We simply convert the i32 value to string here.
//         self.0.clone()
//     }
//     fn emphasize(&self) -> bool {
//         false
//     }
// }
