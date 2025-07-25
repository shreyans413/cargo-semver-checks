use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use itertools::Itertools;
use serde::Serialize;
use tame_index::IndexKrate;
use trustfall_rustdoc::VersionedStorage;

use crate::GlobalConfig;
use crate::data_generation::{CrateDataRequest, IntoTerminalResult as _, TerminalError};
use crate::manifest::Manifest;

#[derive(Debug, Clone)]
pub(crate) enum CrateSource<'a> {
    Registry {
        versioned_krate: &'a tame_index::IndexVersion,
    },
    ManifestPath {
        manifest: &'a Manifest,
    },
}

impl CrateSource<'_> {
    /// Returns features listed in `[features]` section in the manifest
    /// <https://doc.rust-lang.org/cargo/reference/features.html#the-features-section>
    pub(crate) fn regular_features(&self) -> Vec<String> {
        match self {
            Self::Registry {
                versioned_krate, ..
            } => versioned_krate
                .features()
                .map(|(k, _v)| k)
                .cloned()
                .collect(),
            Self::ManifestPath { manifest } => manifest.parsed.features.keys().cloned().collect(),
        }
    }

    /// Returns features implicitly defined by optional dependencies
    /// <https://doc.rust-lang.org/cargo/reference/features.html#optional-dependencies>
    pub(crate) fn implicit_features(&self) -> std::collections::BTreeSet<String> {
        let mut implicit_features: std::collections::BTreeSet<_> = match self {
            Self::Registry {
                versioned_krate, ..
            } => versioned_krate
                .dependencies()
                .iter()
                .filter(|dep| dep.is_optional())
                .map(|dep| dep.name.to_string())
                .collect(),
            Self::ManifestPath { manifest } => {
                let mut dependencies = manifest.parsed.dependencies.clone();
                for target in manifest.parsed.target.values() {
                    // Fixes https://github.com/obi1kenobi/cargo-semver-checks/issues/369
                    // This part is not relevant to `Self::Registry`, because
                    // it doesn't have a `target` field and doesn't differentiate dependencies
                    // between different targets.
                    dependencies.extend(target.dependencies.clone());
                }
                dependencies
                    .iter()
                    .filter_map(|(name, dep)| {
                        if dep.optional() {
                            Some(name.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            }
        };

        let feature_defns: Vec<&String> = match self {
            Self::Registry {
                versioned_krate, ..
            } => versioned_krate.features().flat_map(|(_k, v)| v).collect(),
            Self::ManifestPath { manifest } => {
                manifest.parsed.features.values().flatten().collect()
            }
        };

        for feature_defn in feature_defns {
            // "If you specify the optional dependency with the dep: prefix anywhere
            //  in the [features] table, that disables the implicit feature."
            // https://doc.rust-lang.org/cargo/reference/features.html#optional-dependencies
            if let Some(optional_dep) = feature_defn.strip_prefix("dep:") {
                implicit_features.remove(optional_dep);
            }
        }
        implicit_features
    }

    /// Sometimes crates ship types with fields or variants that are included
    /// only when certain features are enabled.
    ///
    /// By default, we want to generate rustdoc with `--all-features`,
    /// but that option isn't available outside of the current crate,
    /// so we have to implement it ourselves.
    pub(crate) fn all_features(&self) -> Vec<String> {
        // Implicit features from optional dependencies have to be added separately
        // from regular features: https://github.com/obi1kenobi/cargo-semver-checks/issues/265
        let mut all_crate_features = self.implicit_features();
        all_crate_features.extend(self.regular_features());
        all_crate_features.into_iter().collect()
    }

    /// Sometimes crates include features that are not meant for public use
    /// or otherwise don't adhere to semver:
    ///  - private features, like `bench` used for internal benchmarking only
    ///  - nightly-only features, like `nightly`
    ///  - unstable features containing experimental code, like `unstable`
    ///
    /// To ensure the best possible out-of-the-box user experience,
    /// this function attempts to heuristically exclude feature names like above.
    ///
    /// The heuristics are based on the name since cargo does not currently include
    /// a mechanism for marking features as private/hidden/unstable. When such
    /// mechanisms are available in cargo, we'll update this functionality to make
    /// use of them. Relevant cargo issues:
    /// - unstable/nightly-only features: <https://github.com/rust-lang/cargo/issues/10881>
    /// - private/hidden features:        <https://github.com/rust-lang/cargo/issues/10882>
    ///
    /// Because of the above, this function filters out features with names:
    /// - `unstable`
    /// - `nightly`
    /// - `bench`
    /// - `no_std`
    ///   and any features with prefix:
    /// - `_`
    /// - `unstable_`
    /// - `unstable-`
    fn heuristically_included_features(&self) -> Vec<String> {
        let features_ignored_by_default = std::collections::HashSet::from([
            String::from("unstable"),
            String::from("nightly"),
            String::from("bench"),
            String::from("no_std"),
        ]);

        let prefix_ignored_by_default = ["_", "unstable-", "unstable_"];

        let filter_feature_names =
            |feature_name: &String| !features_ignored_by_default.contains(feature_name);

        let filter_feature_prefix = |feature_name: &String| {
            !prefix_ignored_by_default
                .iter()
                .any(|p| feature_name.starts_with(p))
        };

        self.all_features()
            .into_iter()
            .filter(filter_feature_names)
            .filter(filter_feature_prefix)
            .collect()
    }

    /// Returns features to explicitly enable. Does not fetch default features,
    /// which are enabled separately.
    ///
    /// For baseline version, the extra features that do not exist are ignored,
    /// because they could be just added to the current version.
    /// A warning is issued in this case.
    pub(crate) fn feature_list_from_config(
        &self,
        global_config: &mut GlobalConfig,
        feature_config: &FeatureConfig,
    ) -> Vec<String> {
        let all_features: std::collections::HashSet<String> =
            self.all_features().into_iter().collect();

        let result = [
            match feature_config.features_group {
                FeaturesGroup::All => self.all_features(),
                FeaturesGroup::Heuristic => self.heuristically_included_features(),
                FeaturesGroup::Default | FeaturesGroup::None => vec![],
            },
            feature_config.extra_features.clone(),
        ]
        .concat();

        result
            .into_iter()
            .filter(|feature_name| {
                if !all_features.contains(feature_name) && feature_config.is_baseline {
                    global_config
                        .shell_warn(format!(
                            "Feature `{feature_name}` is not present in the baseline."
                        ))
                        .expect("print failed");
                    false
                } else {
                    true
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CrateType {
    Current,
    Baseline {
        /// When the baseline is being generated from registry
        /// and no specific version was chosen, we want to select a version
        /// that is the same or older than the version of the current crate.
        highest_allowed_version: Option<semver::Version>,
    },
}

/// Configuration used to choose features to enable.
/// Separate configs are used for baseline and current versions.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct FeatureConfig {
    /// Feature set chosen as the foundation.
    pub(crate) features_group: FeaturesGroup,
    /// Explicitly enabled features.
    pub(crate) extra_features: Vec<String>,
    pub(crate) is_baseline: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) enum FeaturesGroup {
    All,
    Default,
    Heuristic,
    None,
}

impl FeatureConfig {
    pub(crate) fn default_for_current() -> Self {
        // The default behaviour for both version is the heuristic approach.
        Self {
            features_group: FeaturesGroup::Heuristic,
            extra_features: Vec::new(),
            is_baseline: false,
        }
    }

    pub(crate) fn default_for_baseline() -> Self {
        Self {
            features_group: FeaturesGroup::Heuristic,
            extra_features: Vec::new(),
            is_baseline: true,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CrateDataForRustdoc<'a> {
    pub(crate) crate_type: CrateType,
    pub(crate) name: String,
    pub(crate) feature_config: &'a FeatureConfig,
    pub(crate) build_target: Option<&'a str>,
}

pub(crate) fn generate_data_request<'a>(
    config: &mut GlobalConfig,
    crate_source: CrateSource<'a>,
    crate_data: &CrateDataForRustdoc<'a>,
) -> CrateDataRequest<'a> {
    let extra_features: BTreeSet<Cow<'_, str>> = crate_source
        .feature_list_from_config(config, crate_data.feature_config)
        .into_iter()
        .map(Cow::Owned)
        .collect();
    let default_features = matches!(
        crate_data.feature_config.features_group,
        FeaturesGroup::All | FeaturesGroup::Default | FeaturesGroup::Heuristic
    );

    match crate_source {
        CrateSource::Registry {
            versioned_krate, ..
        } => CrateDataRequest::from_index(
            versioned_krate,
            default_features,
            extra_features,
            crate_data.build_target,
            matches!(
                crate_data.crate_type,
                crate::rustdoc_gen::CrateType::Baseline { .. }
            ),
        ),
        CrateSource::ManifestPath { manifest } => CrateDataRequest::from_local_project(
            manifest,
            default_features,
            extra_features,
            crate_data.build_target,
            matches!(
                crate_data.crate_type,
                crate::rustdoc_gen::CrateType::Baseline { .. }
            ),
        ),
    }
}

fn terminal_context<C>(err: TerminalError, context: C) -> TerminalError
where
    C: std::fmt::Display + Send + Sync + 'static,
{
    match err {
        TerminalError::WithAdvice(err, advice) => {
            TerminalError::WithAdvice(err.context(context), advice)
        }
        TerminalError::Other(err) => TerminalError::Other(err.context(context)),
    }
}

pub(crate) fn generate_rustdoc(
    config: &mut GlobalConfig,
    generation_settings: super::data_generation::GenerationSettings,
    cache_settings: super::data_generation::CacheSettings<()>,
    target_root: PathBuf,
    data_request: &CrateDataRequest<'_>,
) -> Result<VersionedStorage, TerminalError> {
    let cache_dir = target_root.join("cache");
    let cache_settings = cache_settings.with_path(cache_dir.as_path());

    let mut callbacks = crate::callbacks::Callbacks::new(config);
    data_request.resolve(
        &target_root,
        cache_settings,
        generation_settings,
        &mut callbacks,
    )
}

pub(crate) enum RustdocGenerator {
    File(RustdocFromFile),
    ProjectRoot(RustdocFromProjectRoot),
    GitRevision(RustdocFromGitRevision),
    Registry(RustdocFromRegistry),
}

impl From<RustdocFromFile> for RustdocGenerator {
    fn from(value: RustdocFromFile) -> Self {
        Self::File(value)
    }
}

impl From<RustdocFromProjectRoot> for RustdocGenerator {
    fn from(value: RustdocFromProjectRoot) -> Self {
        Self::ProjectRoot(value)
    }
}

impl From<RustdocFromGitRevision> for RustdocGenerator {
    fn from(value: RustdocFromGitRevision) -> Self {
        Self::GitRevision(value)
    }
}

impl From<RustdocFromRegistry> for RustdocGenerator {
    fn from(value: RustdocFromRegistry) -> Self {
        Self::Registry(value)
    }
}

/// A rustdoc generator state machine, to progress through the states of processing
pub(crate) struct StatefulRustdocGenerator<'a, S> {
    coupled_state: S,
    crate_data: &'a CrateDataForRustdoc<'a>,
}

pub(crate) enum CoupledState<'a> {
    File {
        generator: &'a RustdocFromFile,
    },
    ProjectRoot {
        generator: &'a RustdocFromProjectRoot,
    },
    // GitRevision variant exists purely for improved errors
    GitRevision {
        generator: &'a RustdocFromGitRevision,
    },
    // Registry requests need a list of crate versions to query
    Registry {
        generator: &'a RustdocFromRegistry,
        krate: tame_index::IndexKrate,
    },
}

pub(crate) enum ReadyState<'a> {
    // File source is maintained
    File {
        generator: &'a RustdocFromFile,
    },

    // These are the only values needed for rustdoc generation, and generation operations are not
    // generator specific, meaning fallible operations will not gain additional context from the
    // additional source context of multiple variants
    Generator {
        target_root: &'a PathBuf,
        data_request: CrateDataRequest<'a>,
    },
}

impl<S> StatefulRustdocGenerator<'_, S> {
    /// Retrieve the crate data for this generator
    pub(crate) fn get_crate_data(&self) -> &CrateDataForRustdoc<'_> {
        self.crate_data
    }
}

impl<'a> StatefulRustdocGenerator<'a, CoupledState<'a>> {
    /// Prepare a [`RustdocGenerator`] for rustdoc generation, coupling it with necessary data.
    pub(crate) fn couple_data(
        generator: &'a RustdocGenerator,
        config: &mut GlobalConfig,
        crate_data: &'a CrateDataForRustdoc<'a>,
    ) -> Result<Self, TerminalError> {
        let coupled_data = match generator {
            RustdocGenerator::File(generator) => CoupledState::File { generator },

            RustdocGenerator::ProjectRoot(generator) => CoupledState::ProjectRoot { generator },

            RustdocGenerator::GitRevision(generator) => CoupledState::GitRevision { generator },

            RustdocGenerator::Registry(generator) => {
                let krate = generator.get_krate(config, crate_data).map_err(|err| {
                    terminal_context(
                        err,
                        "failed to retrieve index of crate versions from registry",
                    )
                })?;
                CoupledState::Registry { generator, krate }
            }
        };

        Ok(Self {
            coupled_state: coupled_data,
            crate_data,
        })
    }

    /// Prepare a [`CoupledState`] for rustdoc generation, extracting necessary internal data,
    /// and creating an appropriate data request
    pub(crate) fn prepare_generator(
        &self,
        config: &mut GlobalConfig,
    ) -> Result<StatefulRustdocGenerator<'_, ReadyState<'_>>, TerminalError> {
        let crate_data = self.crate_data;

        let (crate_source, target_root) = match &self.coupled_state {
            CoupledState::File { generator } => {
                return Ok(StatefulRustdocGenerator {
                    coupled_state: ReadyState::File { generator },
                    crate_data,
                });
            }
            CoupledState::ProjectRoot { generator } => {
                let source = generator
                    .get_crate_source(crate_data)
                    .map_err(|err| terminal_context(err, "failed to retrieve local crate data"))?;
                (source, &generator.target_root)
            }
            CoupledState::GitRevision { generator } => {
                let source = generator.get_crate_source(crate_data).map_err(|err| {
                    terminal_context(err, "failed to retrieve local crate data from git revision")
                })?;
                (source, &generator.path.target_root)
            }
            CoupledState::Registry { generator, krate } => {
                let source = generator
                    .get_crate_source(crate_data, krate)
                    .map_err(|err| {
                        terminal_context(err, "failed to retrieve crate data from registry")
                    })?;
                (source, &generator.target_root)
            }
        };

        let data_request = generate_data_request(config, crate_source, crate_data);

        Ok(StatefulRustdocGenerator {
            coupled_state: ReadyState::Generator {
                target_root,
                data_request,
            },
            crate_data,
        })
    }
}

impl<'a> StatefulRustdocGenerator<'a, ReadyState<'a>> {
    // TODO: Use the data request in the witness system
    #[expect(dead_code)]
    /// Get the computed data request for this generator, if one exists
    pub(crate) fn get_data_request(&self) -> Option<&CrateDataRequest<'_>> {
        match &self.coupled_state {
            ReadyState::File { .. } => None,
            ReadyState::Generator { data_request, .. } => Some(data_request),
        }
    }

    /// Load rustdoc from this generator into a [`VersionedStorage`]
    pub(crate) fn load_rustdoc(
        &self,
        config: &mut GlobalConfig,
        generation_settings: super::data_generation::GenerationSettings,
        cache_settings: super::data_generation::CacheSettings<()>,
    ) -> Result<VersionedStorage, TerminalError> {
        match &self.coupled_state {
            ReadyState::File { generator } => generator.load_rustdoc(),

            ReadyState::Generator {
                target_root,
                data_request,
            } => generate_rustdoc(
                config,
                generation_settings,
                cache_settings,
                target_root.to_path_buf(),
                data_request,
            ),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RustdocFromFile {
    path: PathBuf,
}

impl RustdocFromFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load_rustdoc(&self) -> Result<VersionedStorage, TerminalError> {
        trustfall_rustdoc::load_rustdoc(&self.path, None)
            .with_context(|| format!("failed to load rustdoc from file at `{:?}`", self.path))
            .into_terminal_result()
    }
}

#[derive(Debug)]
pub(crate) struct RustdocFromProjectRoot {
    project_root: PathBuf,
    manifests: HashMap<String, Manifest>,
    manifest_errors: HashMap<PathBuf, anyhow::Error>,
    duplicate_packages: HashMap<String, Vec<PathBuf>>,
    target_root: PathBuf,
}

impl RustdocFromProjectRoot {
    /// # Arguments
    /// * `project_root` - Path to a directory with the manifest or with subdirectories with the manifests.
    /// * `target_root` - Path to a directory where the placeholder manifest / rustdoc can be created.
    pub(crate) fn new(
        project_root: &std::path::Path,
        target_root: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let mut manifests_by_path: HashMap<PathBuf, Manifest> = HashMap::new();
        let mut manifest_errors = HashMap::new();

        // First, scan the contents of the root directory for `Cargo.toml` files.
        // Parse such files' contents into `Manifest` values.
        for result in ignore::Walk::new(project_root) {
            let entry = result?;
            if entry.file_name() == "Cargo.toml" {
                let path = entry.into_path();
                match crate::manifest::Manifest::parse(path.clone()) {
                    Ok(manifest) => {
                        manifests_by_path.insert(path, manifest);
                    }
                    Err(e) => {
                        manifest_errors.insert(path, e);
                    }
                }
            }
        }

        // Then, figure out which packages are defined by those manifests.
        // If some package name is defined by more than one manifest, record an error.
        let mut package_manifests: HashMap<String, (PathBuf, Manifest)> = HashMap::new();
        let mut duplicate_packages: HashMap<String, Vec<PathBuf>> = Default::default();
        for (path, manifest) in manifests_by_path.into_iter() {
            let name = match crate::manifest::get_package_name(&manifest) {
                Ok(name) => name.to_string(),
                Err(e) => {
                    manifest_errors.insert(path.clone(), e);
                    continue;
                }
            };

            if let Some(duplicates) = duplicate_packages.get_mut(&name) {
                // This package is defined in multiple manifests already.
                // Add to the list of duplicate manifests that define it.
                duplicates.push(path);
            } else if let Some((prev_path, _)) =
                package_manifests.insert(name.clone(), (path, manifest))
            {
                // This is the first duplicate entry for this package.
                // Remove it from the `package_manifests` and add both
                // conflicting manifests to the duplicates list.
                let (path, _) = package_manifests
                    .remove(&name)
                    .expect("elements we just inserted weren't present");
                duplicate_packages.insert(name, vec![prev_path, path]);
            }
        }
        for (_package, paths) in duplicate_packages.iter_mut() {
            paths.sort_unstable();
        }

        let manifests = package_manifests
            .into_iter()
            .map(|(package, (_, manifest))| (package, manifest))
            .collect();

        Ok(Self {
            project_root: project_root.to_owned(),
            manifests,
            manifest_errors,
            duplicate_packages,
            target_root: target_root.to_owned(),
        })
    }

    fn get_crate_source(
        &self,
        crate_data: &CrateDataForRustdoc<'_>,
    ) -> Result<CrateSource<'_>, TerminalError> {
        let manifest = self.manifests.get(&crate_data.name).ok_or_else(|| {
            if let Some(duplicates) = self.duplicate_packages.get(&crate_data.name) {
                let duplicates = duplicates.iter().map(|p| p.display()).join("\n  ");
                let err = anyhow::anyhow!(
                    "package `{}` is ambiguous: it is defined by in multiple manifests within the root path {}\n\ndefined in:\n  {duplicates}",
                    crate_data.name,
                    self.project_root.display(),
                );
                err
            } else {
                let err = anyhow::anyhow!(
                    "package `{}` not found in {}",
                    crate_data.name,
                    self.project_root.display(),
                );
                if self.manifest_errors.is_empty() {
                    err
                } else {
                    let cause_list = self
                        .manifest_errors
                        .values()
                        .map(|error| format!("  {error:#},"))
                        .join("\n");
                    let possible_causes = format!("possibly due to errors: [\n{cause_list}\n]");
                    err.context(possible_causes)
                }
            }
        }).into_terminal_result()?;

        Ok(CrateSource::ManifestPath { manifest })
    }
}

#[derive(Debug)]
pub(crate) struct RustdocFromGitRevision {
    path: RustdocFromProjectRoot,
}

impl RustdocFromGitRevision {
    pub fn with_rev(
        source: &std::path::Path,
        target: &std::path::Path,
        rev: &str,
        config: &mut GlobalConfig,
    ) -> anyhow::Result<Self> {
        config.shell_status("Cloning", rev)?;
        let repo = gix::ThreadSafeRepository::discover_with_environment_overrides(source)
            .map(gix::Repository::from)?;

        let tree_id = repo.rev_parse_single(&*format!("{rev}^{{tree}}"))?;
        let tree_dir = target.join(tree_id.to_string());

        std::fs::create_dir_all(&tree_dir)?;
        extract_tree(tree_id, &tree_dir)?;

        let path = RustdocFromProjectRoot::new(&tree_dir, target)?;
        Ok(Self { path })
    }

    pub(crate) fn get_crate_source(
        &self,
        crate_data: &CrateDataForRustdoc<'_>,
    ) -> Result<CrateSource<'_>, TerminalError> {
        // As a wrapper around RustdocFromProjectRoot, this just serves to provide additional error context
        self.path.get_crate_source(crate_data).map_err(|err| {
            terminal_context(
                err,
                "failed to retrieve manifest file from git revision source",
            )
        })
    }
}

fn extract_tree(tree: gix::Id<'_>, target: &std::path::Path) -> anyhow::Result<()> {
    for entry in tree.object()?.try_into_tree()?.iter() {
        let entry = entry?;
        let mode = entry.mode();
        if mode.is_tree() {
            let path = target.join(bytes2str(entry.filename()));
            std::fs::create_dir_all(&path)?;
            extract_tree(entry.id(), &path)?;
        } else if mode.is_blob() {
            let blob = entry.object()?;
            assert!(
                blob.kind.is_blob(),
                "we are not working on a corrupted repository"
            );
            let path = target.join(bytes2str(entry.filename()));
            let existing = std::fs::read(&path).ok();
            if existing.as_deref() != Some(&blob.data) {
                std::fs::write(&path, &blob.data)?;
            }
        }
    }

    Ok(())
}

// From git2 crate
#[cfg(unix)]
fn bytes2str(b: &[u8]) -> &std::ffi::OsStr {
    use std::os::unix::prelude::*;
    std::ffi::OsStr::from_bytes(b)
}

// From git2 crate
#[cfg(windows)]
fn bytes2str(b: &[u8]) -> &std::ffi::OsStr {
    use std::str;
    std::ffi::OsStr::new(str::from_utf8(b).unwrap())
}

pub(crate) struct RustdocFromRegistry {
    target_root: PathBuf,
    version: Option<semver::Version>,
    index: tame_index::index::ComboIndex,
}

impl core::fmt::Debug for RustdocFromRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RustdocFromRegistry")
            .field("target_root", &self.target_root)
            .field("version", &self.version)
            .field("index", &"<elided>")
            .finish()
    }
}

impl RustdocFromRegistry {
    pub fn new(target_root: &std::path::Path, config: &mut GlobalConfig) -> anyhow::Result<Self> {
        let index_url = tame_index::IndexUrl::crates_io(
            // This is the config root, where .cargo/config.toml configuration files
            // are crawled to determine if crates.io has been source replaced
            // <https://doc.rust-lang.org/cargo/reference/source-replacement.html>
            // if not specified it defaults to the current working directory,
            // which is the same default that cargo uses, though note this can be
            // extremely confusing if one can specify the manifest path of the
            // crate from a different current working directory, though AFAICT
            // this is not how this binary works
            None,
            // If set this overrides the CARGO_HOME that is used for both finding
            // the "global" default config if not overriden during directory
            // traversal to the root, as well as where the various registry
            // indices/git sources are rooted. This is generally only useful
            // for testing
            None,
            // If set, overrides the version of the cargo binary used, this is used
            // as a fallback to determine if the version is 1.70.0+, which means
            // the default crates.io registry to use is the sparse registry, else
            // it is the old git registry
            None,
        )
        .context("failed to obtain crates.io url")?;

        use tame_index::index::{self, ComboIndexCache};

        let index_cache = ComboIndexCache::new(tame_index::IndexLocation::new(index_url))
            .context("failed to open crates.io index cache")?;

        let index: index::ComboIndex = match index_cache {
            ComboIndexCache::Git(git) => {
                let lock = acquire_cargo_global_package_lock(config)?;
                let mut rgi = index::RemoteGitIndex::new(git, &lock)
                    .context("failed to open crates.io git index")?;

                config.shell_status("Updating", "index")?;
                while need_retry(rgi.fetch(&lock))? {
                    config.shell_status("Blocking", "waiting for lock on registry index")?;
                    std::thread::sleep(REGISTRY_BACKOFF);
                }
                drop(lock);

                rgi.into()
            }
            ComboIndexCache::Sparse(sparse) => {
                let client = tame_index::external::reqwest::blocking::Client::builder()
                    .http2_prior_knowledge()
                    .build()
                    .context("failed to build HTTP client")?;
                index::RemoteSparseIndex::new(sparse, client).into()
            }
            _ => bail!("encountered unknown cache type"),
        };

        Ok(Self {
            target_root: target_root.to_owned(),
            version: None,
            index,
        })
    }

    pub fn set_version(&mut self, version: semver::Version) {
        self.version = Some(version);
    }

    pub(crate) fn get_krate(
        &self,
        config: &mut GlobalConfig,
        crate_data: &CrateDataForRustdoc<'_>,
    ) -> Result<IndexKrate, TerminalError> {
        let lock = acquire_cargo_global_package_lock(config).into_terminal_result()?;
        let validated_name = crate_data
            .name
            .as_str()
            .try_into()
            .expect("this should be impossible");
        let krate = self.index.krate(validated_name, false, &lock)
            .with_context(|| {
                format!("failed to read index metadata for crate '{}'", crate_data.name)
            }).into_terminal_result()?
            .with_context(|| {
            anyhow::format_err!(
                "{} not found in registry (crates.io). \
        For workarounds check \
        https://github.com/obi1kenobi/cargo-semver-checks#does-the-crate-im-checking-have-to-be-published-on-cratesio",
                crate_data.name
            )
        }).into_terminal_result()?;
        drop(lock);

        Ok(krate)
    }

    fn get_crate_source<'a>(
        &self,
        crate_data: &CrateDataForRustdoc<'_>,
        krate: &'a IndexKrate,
    ) -> Result<CrateSource<'a>, TerminalError> {
        let base_version = if let Some(base) = &self.version {
            base.clone()
        } else {
            choose_baseline_version(
                krate,
                match &crate_data.crate_type {
                    CrateType::Current => None,
                    CrateType::Baseline {
                        highest_allowed_version,
                    } => highest_allowed_version.as_ref(),
                },
            )
            .into_terminal_result()?
        };

        let versioned_krate = krate
            .versions
            .iter()
            .find(|v| {
                semver::Version::parse(v.version.as_str()).ok().as_ref() == Some(&base_version)
            })
            .with_context(|| {
                anyhow::format_err!(
                    "crate {} version {} not found in registry",
                    crate_data.name,
                    base_version
                )
            })
            .into_terminal_result()?;

        Ok(CrateSource::Registry { versioned_krate })
    }
}

fn choose_baseline_version(
    krate: &IndexKrate,
    version_current: Option<&semver::Version>,
) -> anyhow::Result<semver::Version> {
    // Try to avoid pre-releases
    // - Breaking changes are allowed between them
    // - Most likely the user cares about the last official release
    if let Some(current) = version_current {
        let mut instances = krate
            .versions
            .iter()
            .map(|iv| (iv.version.clone(), iv.is_yanked()))
            // For unpublished changes when the user doesn't increment the version
            // post-release, allow using the current version as a baseline.
            .filter_map(|(v, yanked)| semver::Version::parse(v.as_str()).ok().map(|v| (v, yanked)))
            .filter(|(v, _)| v <= current)
            .collect::<Vec<_>>();
        instances.sort();
        instances
            .iter()
            .rev()
            .find(|(v, yanked)| v.pre.is_empty() && !yanked)
            .or_else(|| instances.last())
            .map(|(v, _)| v.clone())
            .with_context(|| {
                anyhow::format_err!(
                    "No available baseline versions for {}@{}",
                    krate.name(),
                    current
                )
            })
    } else {
        let instance = semver::Version::parse(
            krate
                .highest_normal_version()
                .unwrap_or_else(|| {
                    // If there is no normal version (not yanked and not a pre-release)
                    // choosing the latest one anyway is more reasonable than throwing an
                    // error, as there is still a chance that it is what the user expects.
                    krate.highest_version()
                })
                .version
                .as_str(),
        )?;
        Ok(instance)
    }
}

const REGISTRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Check if we need to retry retrieving the Index.
fn need_retry(res: Result<(), tame_index::Error>) -> anyhow::Result<bool> {
    match res {
        Ok(()) => Ok(false),
        Err(tame_index::Error::Git(err)) => {
            if err.is_spurious() || err.is_locked() {
                Ok(true)
            } else {
                Err(tame_index::Error::Git(err).into())
            }
        }
        Err(err) => Err(err.into()),
    }
}

fn acquire_cargo_global_package_lock(
    config: &mut GlobalConfig,
) -> anyhow::Result<tame_index::index::FileLock> {
    let lock_options = tame_index::utils::flock::LockOptions::cargo_package_lock(None)
        .expect("failed to create the global cargo package lock, is $CARGO_HOME set?");
    match lock_options.try_lock() {
        Ok(lock) => Ok(lock),
        Err(_) => {
            config.shell_status("Blocking", "waiting for cargo global package lock")?;
            Ok(lock_options.lock(|_| None)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use tame_index::{IndexKrate, IndexVersion};

    use super::choose_baseline_version;

    fn new_mock_version(version: semver::Version, yanked: bool) -> IndexVersion {
        let mut iv = IndexVersion::fake("test-crate", version.to_string());
        iv.yanked = yanked;
        iv
    }

    fn assert_correctly_picks_baseline_version(
        versions: Vec<(&str, bool)>,
        current_version_name: Option<&str>,
        expected: &str,
    ) {
        let krate = IndexKrate {
            versions: versions
                .into_iter()
                .map(|(version, yanked)| new_mock_version(version.parse().unwrap(), yanked))
                .collect(),
        };
        let current_version = current_version_name.map(|version_name| {
            semver::Version::parse(version_name)
                .expect("current_version_name used in assertion should encode a valid version")
        });
        let chosen_baseline = choose_baseline_version(&krate, current_version.as_ref())
            .expect("choose_baseline_version should not return any error in the test case");
        assert_eq!(chosen_baseline, expected.parse().unwrap());
    }

    #[test]
    fn baseline_choosing_logic_skips_yanked() {
        assert_correctly_picks_baseline_version(
            vec![("1.2.0", false), ("1.2.1", true)],
            Some("1.2.2"),
            "1.2.0",
        );
    }

    #[test]
    fn baseline_choosing_logic_skips_greater_than_current() {
        assert_correctly_picks_baseline_version(
            vec![("1.2.0", false), ("1.2.1", false)],
            Some("1.2.0"),
            "1.2.0",
        );
    }

    #[test]
    fn baseline_choosing_logic_skips_pre_releases() {
        assert_correctly_picks_baseline_version(
            vec![("1.2.0", false), ("1.2.1-rc1", false)],
            Some("1.2.1-rc2"),
            "1.2.0",
        );
    }

    #[test]
    fn baseline_choosing_logic_without_current_picks_latest_normal() {
        assert_correctly_picks_baseline_version(
            vec![("1.2.0", false), ("1.2.1-rc1", false), ("1.3.1", true)],
            None,
            "1.2.0",
        );
    }

    #[test]
    fn baseline_choosing_logic_picks_pre_release_if_there_is_no_normal() {
        assert_correctly_picks_baseline_version(
            vec![("1.2.0", true), ("1.2.1-rc1", false)],
            Some("1.2.1"),
            "1.2.1-rc1",
        );
    }

    #[test]
    fn baseline_choosing_logic_picks_yanked_if_there_is_no_normal() {
        assert_correctly_picks_baseline_version(
            vec![("1.2.1-rc1", false), ("1.2.1", true)],
            Some("1.2.1"),
            "1.2.1",
        );
    }
}
