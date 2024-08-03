mod archive;

use anyhow::{anyhow, bail, Context, Result};
pub use archive::extract_zip;
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::AsyncReadExt;
use gpui::AppContext;
use http_client::HttpClient;
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsSources, SettingsStore};
use smol::io::BufReader;
use smol::{fs, lock::Mutex, process::Command};
use std::io;
use std::process::{Output, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::{
    env::consts,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt;

#[cfg(windows)]
use smol::process::windows::CommandExt;

const VERSION: &str = "v22.5.1";

#[cfg(not(windows))]
const NODE_PATH: &str = "bin/node";
#[cfg(windows)]
const NODE_PATH: &str = "node.exe";

#[cfg(not(windows))]
const NPM_PATH: &str = "bin/npm";
#[cfg(windows)]
const NPM_PATH: &str = "node_modules/npm/bin/npm-cli.js";

enum ArchiveType {
    TarGz,
    Zip,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NpmInfo {
    #[serde(default)]
    dist_tags: NpmInfoDistTags,
    versions: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct NpmInfoDistTags {
    latest: Option<String>,
}

#[async_trait::async_trait]
pub trait NodeRuntime: Send + Sync {
    async fn binary_path(&self) -> Result<PathBuf>;

    fn configure(&self, settings: NodeRuntimeSettings);

    async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output>;

    async fn npm_package_latest_version(&self, name: &str) -> Result<String>;

    async fn npm_install_packages(&self, directory: &Path, packages: &[(&str, &str)])
        -> Result<()>;

    async fn npm_package_installed_version(
        &self,
        local_package_directory: &PathBuf,
        name: &str,
    ) -> Result<Option<String>>;

    async fn should_install_npm_package(
        &self,
        package_name: &str,
        local_executable_path: &Path,
        local_package_directory: &PathBuf,
        latest_version: &str,
    ) -> bool {
        // In the case of the local system not having the package installed,
        // or in the instances where we fail to parse package.json data,
        // we attempt to install the package.
        if fs::metadata(local_executable_path).await.is_err() {
            return true;
        }

        let Some(installed_version) = self
            .npm_package_installed_version(local_package_directory, package_name)
            .await
            .log_err()
            .flatten()
        else {
            return true;
        };

        let Some(installed_version) = Version::parse(&installed_version).log_err() else {
            return true;
        };
        let Some(latest_version) = Version::parse(&latest_version).log_err() else {
            return true;
        };

        installed_version < latest_version
    }
}

pub struct RealNodeRuntime {
    http: Arc<dyn HttpClient>,
    settings: Mutex<(NodeRuntimeSettings, Receiver<NodeRuntimeSettings>)>,
    pending_settings: Sender<NodeRuntimeSettings>,
}

impl RealNodeRuntime {
    pub fn new(http: Arc<dyn HttpClient>) -> Arc<dyn NodeRuntime> {
        let (sender, receiver) = std::sync::mpsc::channel();
        Arc::new(RealNodeRuntime {
            http,
            settings: Mutex::new((Default::default(), receiver)),
            pending_settings: sender,
        })
    }

    async fn install_if_needed(&self) -> Result<NodePaths> {
        let mut lock = self.settings.lock().await;

        while let Ok(pending) = lock.1.try_recv() {
            lock.0 = pending;
        }

        log::info!("Node runtime install_if_needed");

        let os = match consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            "windows" => "win",
            other => bail!("Running on unsupported os: {other}"),
        };

        let arch = match consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => bail!("Running on unsupported architecture: {other}"),
        };

        let settings = &lock.0;
        let has_override = settings.npm.is_some() || settings.node.is_some();

        let folder_name = format!("node-{VERSION}-{os}-{arch}");
        let node_containing_dir = paths::support_dir().join("node");
        let node_dir = node_containing_dir.join(folder_name);
        let paths = NodePaths {
            node: settings
                .node
                .clone()
                .unwrap_or_else(|| node_dir.join(NODE_PATH)),
            npm: settings
                .npm
                .clone()
                .unwrap_or_else(|| node_dir.join(NPM_PATH)),
            cache: settings
                .cache
                .clone()
                .unwrap_or_else(|| node_dir.join("cache")),
        };

        let mut command = paths.create_npm_command();

        command
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);

        let result = command.status().await;
        let valid = matches!(result, Ok(status) if status.success());

        if !valid {
            if has_override {
                bail!("node override {:?} could not be executed", paths.node);
            }

            _ = fs::remove_dir_all(&node_containing_dir).await;
            fs::create_dir(&node_containing_dir)
                .await
                .context("error creating node containing dir")?;

            let archive_type = match consts::OS {
                "macos" | "linux" => ArchiveType::TarGz,
                "windows" => ArchiveType::Zip,
                other => bail!("Running on unsupported os: {other}"),
            };

            let file_name = format!(
                "node-{VERSION}-{os}-{arch}.{extension}",
                extension = match archive_type {
                    ArchiveType::TarGz => "tar.gz",
                    ArchiveType::Zip => "zip",
                }
            );
            let url = format!("https://nodejs.org/dist/{VERSION}/{file_name}");
            let mut response = self
                .http
                .get(&url, Default::default(), true)
                .await
                .context("error downloading Node binary tarball")?;

            let body = response.body_mut();
            match archive_type {
                ArchiveType::TarGz => {
                    let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
                    let archive = Archive::new(decompressed_bytes);
                    archive.unpack(&node_containing_dir).await?;
                }
                ArchiveType::Zip => archive::extract_zip(&node_containing_dir, body).await?,
            }
        }

        // Note: Not in the `if !valid {}` so we can populate these for existing installations
        _ = fs::create_dir(&paths.cache).await;
        _ = fs::write(paths.user_rc(), []).await;
        _ = fs::write(paths.global_rc(), []).await;

        anyhow::Ok(paths)
    }
}

#[async_trait::async_trait]
impl NodeRuntime for RealNodeRuntime {
    async fn binary_path(&self) -> Result<PathBuf> {
        let paths = self.install_if_needed().await?;
        Ok(paths.node.clone())
    }

    fn configure(&self, settings: NodeRuntimeSettings) {
        self.pending_settings.send(settings).ok();
    }

    async fn run_npm_subcommand(
        &self,
        directory: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> Result<Output> {
        let attempt = || async move {
            let paths = self.install_if_needed().await?;

            let mut env_path = vec![
                paths
                    .node
                    .parent()
                    .expect("invalid node binary path")
                    .to_path_buf(),
                paths
                    .npm
                    .parent()
                    .expect("invalid npm binary path")
                    .to_path_buf(),
            ];

            if let Some(existing_path) = std::env::var_os("PATH") {
                let mut paths = std::env::split_paths(&existing_path).collect::<Vec<_>>();
                env_path.append(&mut paths);
            }

            let env_path =
                std::env::join_paths(env_path).context("failed to create PATH env variable")?;

            if smol::fs::metadata(&paths.node).await.is_err() {
                return Err(anyhow!("missing node binary file"));
            }

            if smol::fs::metadata(&paths.npm).await.is_err() {
                return Err(anyhow!("missing npm file"));
            }

            let mut command = paths.create_npm_command();
            command.env("PATH", env_path);
            command.arg(subcommand);
            command.args(args);

            if let Some(directory) = directory {
                command.current_dir(directory);
                command.args(["--prefix".into(), directory.to_path_buf()]);
            }

            if let Some(proxy) = self.http.proxy() {
                // Map proxy settings from `http://localhost:10809` to `http://127.0.0.1:10809`
                // NodeRuntime without environment information can not parse `localhost`
                // correctly.
                // TODO: map to `[::1]` if we are using ipv6
                let proxy = proxy
                    .to_string()
                    .to_ascii_lowercase()
                    .replace("localhost", "127.0.0.1");

                command.args(["--proxy", &proxy]);
            }

            #[cfg(windows)]
            {
                // SYSTEMROOT is a critical environment variables for Windows.
                if let Some(val) = std::env::var("SYSTEMROOT")
                    .context("Missing environment variable: SYSTEMROOT!")
                    .log_err()
                {
                    command.env("SYSTEMROOT", val);
                }
                // Without ComSpec, the post-install will always fail.
                if let Some(val) = std::env::var("ComSpec")
                    .context("Missing environment variable: ComSpec!")
                    .log_err()
                {
                    command.env("ComSpec", val);
                }
                command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
            }

            command.output().await.map_err(|e| anyhow!("{e}"))
        };

        let mut output = attempt().await;
        if output.is_err() {
            output = attempt().await;
            if output.is_err() {
                return Err(anyhow!(
                    "failed to launch npm subcommand {subcommand} subcommand\nerr: {:?}",
                    output.err()
                ));
            }
        }

        if let Ok(output) = &output {
            if !output.status.success() {
                return Err(anyhow!(
                    "failed to execute npm {subcommand} subcommand:\nstdout: {:?}\nstderr: {:?}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }

        output.map_err(|e| anyhow!("{e}"))
    }

    async fn npm_package_latest_version(&self, name: &str) -> Result<String> {
        let output = self
            .run_npm_subcommand(
                None,
                "info",
                &[
                    name,
                    "--json",
                    "--fetch-retry-mintimeout",
                    "2000",
                    "--fetch-retry-maxtimeout",
                    "5000",
                    "--fetch-timeout",
                    "5000",
                ],
            )
            .await?;

        let mut info: NpmInfo = serde_json::from_slice(&output.stdout)?;
        info.dist_tags
            .latest
            .or_else(|| info.versions.pop())
            .ok_or_else(|| anyhow!("no version found for npm package {}", name))
    }

    async fn npm_package_installed_version(
        &self,
        local_package_directory: &PathBuf,
        name: &str,
    ) -> Result<Option<String>> {
        let mut package_json_path = local_package_directory.clone();
        package_json_path.extend(["node_modules", name, "package.json"]);

        let mut file = match fs::File::open(package_json_path).await {
            Ok(file) => file,
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    return Ok(None);
                }

                Err(err)?
            }
        };

        #[derive(Deserialize)]
        struct PackageJson {
            version: String,
        }

        let mut contents = String::new();
        file.read_to_string(&mut contents).await?;
        let package_json: PackageJson = serde_json::from_str(&contents)?;
        Ok(Some(package_json.version))
    }

    async fn npm_install_packages(
        &self,
        directory: &Path,
        packages: &[(&str, &str)],
    ) -> Result<()> {
        let packages: Vec<_> = packages
            .into_iter()
            .map(|(name, version)| format!("{name}@{version}"))
            .collect();

        let mut arguments: Vec<_> = packages.iter().map(|p| p.as_str()).collect();
        arguments.extend_from_slice(&[
            "--save-exact",
            "--fetch-retry-mintimeout",
            "2000",
            "--fetch-retry-maxtimeout",
            "5000",
            "--fetch-timeout",
            "5000",
        ]);

        self.run_npm_subcommand(Some(directory), "install", &arguments)
            .await?;
        Ok(())
    }
}

pub struct FakeNodeRuntime;

impl FakeNodeRuntime {
    pub fn new() -> Arc<dyn NodeRuntime> {
        Arc::new(Self)
    }
}

#[async_trait::async_trait]
impl NodeRuntime for FakeNodeRuntime {
    async fn binary_path(&self) -> anyhow::Result<PathBuf> {
        unreachable!()
    }

    fn configure(&self, _settings: NodeRuntimeSettings) {}

    async fn run_npm_subcommand(
        &self,
        _: Option<&Path>,
        subcommand: &str,
        args: &[&str],
    ) -> anyhow::Result<Output> {
        unreachable!("Should not run npm subcommand '{subcommand}' with args {args:?}")
    }

    async fn npm_package_latest_version(&self, name: &str) -> anyhow::Result<String> {
        unreachable!("Should not query npm package '{name}' for latest version")
    }

    async fn npm_package_installed_version(
        &self,
        _local_package_directory: &PathBuf,
        name: &str,
    ) -> Result<Option<String>> {
        unreachable!("Should not query npm package '{name}' for installed version")
    }

    async fn npm_install_packages(
        &self,
        _: &Path,
        packages: &[(&str, &str)],
    ) -> anyhow::Result<()> {
        unreachable!("Should not install packages {packages:?}")
    }
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NodeRuntimeSettings {
    /// The path to the Node.js binary.
    #[serde(default)]
    pub node: Option<PathBuf>,

    /// The path to the npm binary.
    #[serde(default)]
    pub npm: Option<PathBuf>,

    /// The path to the cache directory.
    #[serde(default)]
    pub cache: Option<PathBuf>,
}

impl Settings for NodeRuntimeSettings {
    const KEY: Option<&'static str> = Some("node_runtime");

    type FileContent = Self;

    fn load(
        sources: SettingsSources<Self::FileContent>,
        _: &mut AppContext,
    ) -> anyhow::Result<Self> {
        sources.json_merge()
    }
}

struct NodePaths {
    pub node: PathBuf,
    pub npm: PathBuf,
    pub cache: PathBuf,
}

impl NodePaths {
    fn user_rc(&self) -> PathBuf {
        self.cache.join("blank_user_npmrc")
    }

    fn global_rc(&self) -> PathBuf {
        self.cache.join("blank_global_npmrc")
    }

    fn create_node_command(&self) -> Command {
        let mut command = Command::new(&self.node);
        command.env_clear();
        command
    }

    fn create_npm_command(&self) -> Command {
        let mut command = self.create_node_command();

        command
            .arg(&self.npm)
            .args(["--cache".into(), self.cache.clone()])
            .args(["--userconfig".into(), self.user_rc()])
            .args(["--globalconfig".into(), self.global_rc()]);

        command
    }
}

pub fn init(node: Arc<dyn NodeRuntime>, cx: &mut AppContext) {
    NodeRuntimeSettings::register(cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = NodeRuntimeSettings::get_global(cx);
        node.configure(settings.clone());
    })
    .detach();
}
