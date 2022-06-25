use super::DataError;
use crate::launcher::ModLoader;
use daedalus::modded::LoaderVersion;
use futures::*;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs,
    process::{Child, Command},
    sync::{Mutex, RwLock, RwLockReadGuard},
};

static PROFILES: OnceCell<RwLock<Profiles>> = OnceCell::new();
pub const PROFILE_JSON_PATH: &str = "profile.json";

#[derive(Debug)]
pub struct Profiles(pub HashMap<PathBuf, Profile>);

// TODO: possibly add defaults to some of these values
pub const CURRENT_FORMAT_VERSION: u32 = 1;
pub const SUPPORTED_ICON_FORMATS: &[&'static str] = &[
    "bmp", "gif", "jpeg", "jpg", "jpe", "png", "svg", "svgz", "webp", "rgb",
    "mp4",
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    #[serde(skip)]
    pub path: PathBuf,
    pub metadata: Metadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java: Option<JavaSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<WindowSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<ProfileHooks>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Metadata {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
    pub game_version: String,
    #[serde(default)]
    pub loader: ModLoader,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<LoaderVersion>,
    pub format_version: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JavaSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_arguments: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct MemorySettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<u32>,
    pub maximum: u32,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            minimum: None,
            maximum: 2048,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct WindowSize(pub u16, pub u16);

impl Default for WindowSize {
    fn default() -> Self {
        Self(854, 480)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProfileHooks {
    #[serde(skip_serializing_if = "HashSet::is_empty", default)]
    pub pre_launch: HashSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrapper: Option<String>,
    #[serde(skip_serializing_if = "HashSet::is_empty", default)]
    pub post_exit: HashSet<String>,
}

impl Default for ProfileHooks {
    fn default() -> Self {
        Self {
            pre_launch: HashSet::<String>::new(),
            wrapper: None,
            post_exit: HashSet::<String>::new(),
        }
    }
}

impl Profile {
    pub async fn new(
        name: String,
        version: String,
        path: PathBuf,
    ) -> Result<Self, DataError> {
        if name.trim().is_empty() {
            return Err(DataError::FormatError(String::from(
                "Empty name for instance!",
            )));
        }

        Ok(Self {
            path: path.canonicalize()?,
            metadata: Metadata {
                name,
                icon: None,
                game_version: version,
                loader: ModLoader::Vanilla,
                loader_version: None,
                format_version: CURRENT_FORMAT_VERSION,
            },
            java: None,
            memory: None,
            resolution: None,
            hooks: None,
        })
    }

    pub async fn run(
        &self,
        credentials: &crate::launcher::Credentials,
    ) -> Result<Child, crate::launcher::LauncherError> {
        use crate::launcher::LauncherError;
        let (settings, version_info) = tokio::try_join! {
            super::Settings::get(),
            super::Metadata::get()
                .and_then(|manifest| async move {
                    let version = manifest
                        .minecraft
                        .versions
                        .iter()
                        .find(|it| it.id == self.metadata.game_version.as_ref())
                        .ok_or_else(|| DataError::FormatError(format!(
                            "invalid or unknown version: {}",
                            self.metadata.game_version
                        )))?;

                    Ok(daedalus::minecraft::fetch_version_info(version)
                       .await?)
                })
        }?;

        let ref pre_launch_hooks =
            self.hooks.as_ref().unwrap_or(&settings.hooks).pre_launch;
        for hook in pre_launch_hooks.iter() {
            // TODO: hook parameters
            let mut cmd = hook.split(' ');
            let result = Command::new(cmd.next().unwrap())
                .args(&cmd.collect::<Vec<&str>>())
                .current_dir(&self.path)
                .spawn()?
                .wait()
                .await?;

            if !result.success() {
                return Err(LauncherError::ExitError(
                    result.code().unwrap_or(-1),
                ));
            }
        }

        let java_install = match self.java {
            Some(JavaSettings {
                install: Some(ref install),
                ..
            }) => install,
            _ => if version_info
                .java_version
                .as_ref()
                .filter(|it| it.major_version >= 16)
                .is_some()
            {
                settings.java_17_path.as_ref()
            } else {
                settings.java_8_path.as_ref()
            }
            .ok_or_else(|| {
                LauncherError::JavaError(format!(
                    "No Java installed for version {}",
                    version_info.java_version.map_or(8, |it| it.major_version),
                ))
            })?,
        };

        if !java_install.exists() {
            return Err(LauncherError::JavaError(format!(
                "Could not find java install: {}",
                java_install.display()
            )));
        }

        let java_args = &self
            .java
            .as_ref()
            .and_then(|it| it.extra_arguments.as_ref())
            .unwrap_or(&settings.custom_java_args);

        let wrapper = self
            .hooks
            .as_ref()
            .map_or(&settings.hooks.wrapper, |it| &it.wrapper);

        let ref memory = self.memory.unwrap_or(settings.memory);
        let ref resolution =
            self.resolution.unwrap_or(settings.game_resolution);

        crate::launcher::launch_minecraft(
            &self.metadata.game_version,
            &self.metadata.loader_version,
            &self.path,
            &java_install,
            &java_args,
            &wrapper,
            memory,
            resolution,
            credentials,
        )
        .await
    }

    pub async fn kill(
        &self,
        running: &mut Child,
    ) -> Result<(), crate::launcher::LauncherError> {
        running.kill().await?;
        self.wait_for(running).await
    }

    pub async fn wait_for(
        &self,
        running: &mut Child,
    ) -> Result<(), crate::launcher::LauncherError> {
        let result = running.wait().await.map_err(|err| {
            crate::launcher::LauncherError::ProcessError {
                inner: err,
                process: String::from("minecraft"),
            }
        })?;

        match result.success() {
            false => Err(crate::launcher::LauncherError::ExitError(
                result.code().unwrap_or(-1),
            )),
            true => Ok(()),
        }
    }

    // TODO: deduplicate these builder methods
    // They are flat like this in order to allow builder-style usage
    pub fn with_name(&mut self, name: String) -> &mut Self {
        self.metadata.name = name;
        self
    }

    pub async fn with_icon(
        &mut self,
        icon: &Path,
    ) -> Result<&mut Self, DataError> {
        let ext = icon
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("");

        if SUPPORTED_ICON_FORMATS.contains(&ext) {
            let file_name = format!("icon.{ext}");
            fs::copy(icon, &self.path.join(&file_name)).await?;
            self.metadata.icon =
                Some(Path::new(&format!("./{file_name}")).to_owned());

            Ok(self)
        } else {
            Err(DataError::FormatError(format!(
                "Unsupported image type: {ext}"
            )))
        }
    }

    pub fn with_game_version(&mut self, version: String) -> &mut Self {
        self.metadata.game_version = version;
        self
    }

    pub fn with_loader(
        &mut self,
        loader: ModLoader,
        version: Option<LoaderVersion>,
    ) -> &mut Self {
        self.metadata.loader = loader;
        self.metadata.loader_version = version;
        self
    }

    pub fn with_java_settings(
        &mut self,
        settings: Option<JavaSettings>,
    ) -> &mut Self {
        self.java = settings;
        self
    }

    pub fn with_memory(
        &mut self,
        settings: Option<MemorySettings>,
    ) -> &mut Self {
        self.memory = settings;
        self
    }

    pub fn with_resolution(
        &mut self,
        resolution: Option<WindowSize>,
    ) -> &mut Self {
        self.resolution = resolution;
        self
    }

    pub fn with_hooks(&mut self, hooks: Option<ProfileHooks>) -> &mut Self {
        self.hooks = hooks;
        self
    }
}

impl Profiles {
    pub async fn init() -> Result<(), DataError> {
        let settings = super::Settings::get().await?;
        let profiles = Arc::new(Mutex::new(HashMap::new()));

        let futures = settings.profiles.clone().into_iter().map(|path| async {
            let profiles = Arc::clone(&profiles);
            tokio::spawn(async move {
                // TODO: handle missing profiles
                let mut profiles = profiles.lock().await;
                let profile = Self::read_profile_from_dir(path.clone()).await?;

                profiles.insert(path, profile);
                Ok(()) as Result<_, DataError>
            })
            .await
            .unwrap()
        });
        futures::future::try_join_all(futures).await?;

        PROFILES.get_or_init(|| {
            RwLock::new(Profiles(
                Arc::try_unwrap(profiles).unwrap().into_inner(),
            ))
        });
        Ok(())
    }

    pub async fn insert(profile: Profile) -> Result<(), DataError> {
        let mut profiles = PROFILES
            .get()
            .ok_or_else(|| {
                DataError::InitializedError(String::from("profiles"))
            })?
            .write()
            .await;

        super::Settings::get_mut()
            .await?
            .profiles
            .insert(profile.path.clone());
        profiles.0.insert(profile.path.clone(), profile);
        Ok(())
    }

    pub async fn insert_from(path: PathBuf) -> Result<(), DataError> {
        Self::read_profile_from_dir(path)
            .and_then(Self::insert)
            .await
    }

    pub async fn remove(path: &Path) -> Result<Option<Profile>, DataError> {
        let path = path.canonicalize()?;
        let mut profiles = PROFILES.get().unwrap().write().await;
        super::Settings::get_mut().await?.profiles.remove(&path);
        Ok(profiles.0.remove(&path))
    }

    pub async fn save() -> Result<(), DataError> {
        let profiles = Self::get().await?;

        let futures = profiles.0.clone().into_iter().map(|(path, profile)| {
            tokio::spawn(async move {
                let json = tokio::task::spawn_blocking(move || {
                    serde_json::to_vec_pretty(&profile)
                })
                .await
                .unwrap()?;

                let profile_json_path = path.join(PROFILE_JSON_PATH);
                fs::write(profile_json_path, json).await?;
                Ok(()) as Result<(), DataError>
            })
        });
        futures::future::try_join_all(futures)
            .await
            .unwrap()
            .into_iter()
            .collect::<Result<_, DataError>>()?;

        Ok(())
    }

    pub async fn get<'a>() -> Result<RwLockReadGuard<'a, Self>, DataError> {
        Ok(PROFILES
            .get()
            .ok_or_else(|| DataError::InitializedError("profiles".to_string()))?
            .read()
            .await)
    }

    async fn read_profile_from_dir(
        path: PathBuf,
    ) -> Result<Profile, DataError> {
        let json = fs::read(path.join(PROFILE_JSON_PATH)).await?;
        let mut profile = serde_json::from_slice::<Profile>(&json)?;
        profile.path = path.clone();
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::{assert_eq, assert_str_eq};

    #[test]
    fn profile_test() -> Result<(), serde_json::Error> {
        let profile = Profile {
            path: PathBuf::from("/tmp/nunya/beeswax"),
            metadata: Metadata {
                name: String::from("Example Pack"),
                icon: None,
                game_version: String::from("1.18.2"),
                loader: ModLoader::Vanilla,
                loader_version: None,
                format_version: CURRENT_FORMAT_VERSION,
            },
            java: JavaSettings {
                install: PathBuf::from("/usr/bin/java"),
                extra_arguments: Vec::new(),
            },
            memory: MemorySettings {
                minimum: None,
                maximum: 8192,
            },
            resolution: WindowSize(1920, 1080),
            hooks: ProfileHooks {
                pre_launch: HashSet::new(),
                wrapper: None,
                post_exit: HashSet::new(),
            },
        };
        let json = serde_json::json!({
            "path": "/tmp/nunya/beeswax",
            "metadata": {
                "name": "Example Pack",
                "game_version": "1.18.2",
                "format_version": 1u32,
            },
            "java": {
              "install": "/usr/bin/java",
            },
            "memory": {
              "maximum": 8192u32,
            },
            "resolution": (1920u16, 1080u16),
            "hooks": {},
        });

        assert_eq!(serde_json::to_value(profile.clone())?, json.clone());
        assert_str_eq!(
            format!("{:?}", serde_json::from_value::<Profile>(json)?),
            format!("{:?}", profile),
        );
        Ok(())
    }
}
