// Copyright 2022 Pants project contributors (see CONTRIBUTORS.md).
// Licensed under the Apache License, Version 2.0 (see LICENSE).
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{self, Debug, Display};
use std::io::Write;
use std::ops::Neg;
use std::os::unix::{fs::OpenOptionsExt, process::ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use deepsize::DeepSizeOf;
use fs::{
    self, DigestTrie, DirectoryDigest, EMPTY_DIRECTORY_DIGEST, GlobExpansionConjunction,
    GlobMatching, PathGlobs, Permissions, RelativePath, StrictGlobMatching, SymlinkBehavior,
    TypedPath,
};
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use futures::{FutureExt, TryFutureExt, try_join};
use log::{debug, info};
use nails::execution::ExitCode;
use sandboxer::Sandboxer;
use serde::Serialize;
use shell_quote::Bash;
use store::{
    ImmutableInputs, OneOffStoreFileByDigest, Snapshot, SnapshotOps, Store, WorkdirSymlink,
};
use task_executor::Executor;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_util::codec::{BytesCodec, FramedRead};
use workunit_store::{Level, Metric, RunningWorkunit, in_workunit};

use crate::fork_exec::spawn_process;
use crate::{
    Context, FallibleProcessResultWithPlatform, ManagedChild, NamedCaches, Process, ProcessError,
    ProcessResultMetadata, ProcessResultSource,
};

pub const USER_EXECUTABLE_MODE: u32 = 0o100755;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, DeepSizeOf, strum_macros::EnumString, Serialize,
)]
#[strum(serialize_all = "snake_case")]
pub enum KeepSandboxes {
    Always,
    Never,
    OnFailure,
}

pub struct CommandRunner {
    pub store: Store,
    sandboxer: Option<Sandboxer>,
    executor: Executor,
    work_dir_base: PathBuf,
    named_caches: NamedCaches,
    immutable_inputs: ImmutableInputs,
    spawn_lock: Arc<RwLock<()>>,
}

impl CommandRunner {
    pub fn new(
        store: Store,
        sandboxer: Option<Sandboxer>,
        executor: Executor,
        work_dir_base: PathBuf,
        named_caches: NamedCaches,
        immutable_inputs: ImmutableInputs,
        spawn_lock: Arc<RwLock<()>>,
    ) -> CommandRunner {
        CommandRunner {
            store,
            sandboxer,
            executor,
            work_dir_base,
            named_caches,
            immutable_inputs,
            spawn_lock,
        }
    }

    pub(crate) async fn construct_output_snapshot(
        store: Store,
        posix_fs: Arc<fs::PosixFS>,
        output_file_paths: BTreeSet<RelativePath>,
        output_dir_paths: BTreeSet<RelativePath>,
    ) -> Result<Snapshot, String> {
        let output_paths = output_dir_paths
            .into_iter()
            .flat_map(|p| {
                let mut dir_glob = {
                    let mut dir = PathBuf::from(p).into_os_string();
                    if dir.is_empty() {
                        dir.push(".")
                    }
                    dir
                };
                let dir = dir_glob.clone();
                dir_glob.push("/**");
                vec![dir, dir_glob]
            })
            .chain(
                output_file_paths
                    .into_iter()
                    .map(|p| PathBuf::from(p).into_os_string()),
            )
            .map(|s| {
                s.into_string()
                    .map_err(|e| format!("Error stringifying output paths: {e:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // TODO: should we error when globs fail?
        let output_globs = PathGlobs::new(
            output_paths,
            StrictGlobMatching::Ignore,
            GlobExpansionConjunction::AllMatch,
        )
        .parse()?;

        let path_stats = posix_fs
            .expand_globs(output_globs, SymlinkBehavior::Aware, None)
            .map_err(|err| format!("Error expanding output globs: {err}"))
            .await?;
        Snapshot::from_path_stats(
            OneOffStoreFileByDigest::new(store, posix_fs, true),
            path_stats,
        )
        .await
    }

    pub fn named_caches(&self) -> &NamedCaches {
        &self.named_caches
    }

    pub fn immutable_inputs(&self) -> &ImmutableInputs {
        &self.immutable_inputs
    }
}

impl Debug for CommandRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("local::CommandRunner")
            .finish_non_exhaustive()
    }
}

// TODO: A Stream that ends with `Exit` is error prone: we should consider creating a Child struct
// similar to nails::server::Child (which is itself shaped like `std::process::Child`).
// See https://github.com/stuhood/nails/issues/1 for more info.
#[derive(Debug, PartialEq, Eq)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(ExitCode),
}

///
/// Collect the outputs of a child process.
///
pub async fn collect_child_outputs<'a>(
    stdout: &'a mut BytesMut,
    stderr: &'a mut BytesMut,
    mut stream: BoxStream<'_, Result<ChildOutput, String>>,
) -> Result<i32, String> {
    let mut exit_code = 1;

    while let Some(child_output_res) = stream.next().await {
        match child_output_res? {
            ChildOutput::Stdout(bytes) => stdout.extend_from_slice(&bytes),
            ChildOutput::Stderr(bytes) => stderr.extend_from_slice(&bytes),
            ChildOutput::Exit(code) => exit_code = code.0,
        };
    }

    Ok(exit_code)
}

#[async_trait]
impl super::CommandRunner for CommandRunner {
    ///
    /// Runs a command on this machine in the passed working directory.
    ///
    async fn run(
        &self,
        context: Context,
        _workunit: &mut RunningWorkunit,
        req: Process,
    ) -> Result<FallibleProcessResultWithPlatform, ProcessError> {
        let req_debug_repr = format!("{req:#?}");
        in_workunit!(
            "run_local_process",
            req.level,
            // NB: See engine::nodes::NodeKey::workunit_level for more information on why this workunit
            // renders at the Process's level.
            desc = Some(req.description.clone()),
            |workunit| async move {
                let keep_sandboxes = req.execution_environment.local_keep_sandboxes;
                let mut workdir = create_sandbox(
                    self.executor.clone(),
                    &self.work_dir_base,
                    &req.description,
                    keep_sandboxes,
                )?;

                // Start working on a mutable version of the process.
                let mut req = req;
                // Update env, replacing `{chroot}` placeholders with `workdir_path`.
                apply_chroot(workdir.path().to_str().unwrap(), &mut req);

                // Prepare the workdir.
                let exclusive_spawn = prepare_workdir(
                    workdir.path().to_owned(),
                    &self.work_dir_base,
                    &req,
                    req.input_digests.inputs.clone(),
                    &self.store,
                    self.sandboxer.as_ref(),
                    &self.named_caches,
                    &self.immutable_inputs,
                    None,
                    None,
                )
                .await?;

                workunit.increment_counter(Metric::LocalExecutionRequests, 1);
                // NB: The constraint on `CapturedWorkdir` is that any child processes spawned here have
                // exited (or been killed in their `Drop` handlers), so this function can rely on the usual
                // Drop order of local variables to assume that the sandbox is cleaned up after the process
                // is.
                let res = self
                    .run_and_capture_workdir(
                        req.clone(),
                        context,
                        self.store.clone(),
                        self.executor.clone(),
                        workdir.path().to_owned(),
                        (),
                        exclusive_spawn,
                    )
                    .map_err(|cwe| {
                        // Processes that experience no infrastructure issues should result in an "Ok" return,
                        // potentially with an exit code that indicates that they failed (with more information
                        // on stderr). Actually failing at this level indicates a failure to start or otherwise
                        // interact with the process, which would generally be an infrastructure or implementation
                        // error (something missing from the sandbox, incorrect permissions, etc).
                        //
                        // Given that this is expected to be rare, we dump the entire process definition in the
                        // error.
                        ProcessError::Unclassified(format!(
                            "Failed to execute: {req_debug_repr}\n\n{cwe}"
                        ))
                    })
                    .await;

                if keep_sandboxes == KeepSandboxes::Always
                    || keep_sandboxes == KeepSandboxes::OnFailure
                        && res.as_ref().map(|r| r.exit_code).unwrap_or(1) != 0
                {
                    workdir.keep(&req.description);
                    setup_run_sh_script(
                        workdir.path(),
                        &req.env,
                        &req.working_directory,
                        &req.argv,
                        workdir.path(),
                    )?;
                }

                res
            }
        )
        .await
    }

    async fn shutdown(&self) -> Result<(), String> {
        Ok(())
    }
}

#[async_trait]
impl CapturedWorkdir for CommandRunner {
    type WorkdirToken = ();

    async fn run_in_workdir<'s, 'c, 'w, 'r>(
        &'s self,
        _context: &'c Context,
        workdir_path: &'w Path,
        _workdir_token: (),
        req: Process,
        exclusive_spawn: bool,
    ) -> Result<BoxStream<'r, Result<ChildOutput, String>>, CapturedWorkdirError> {
        let cwd = if let Some(ref working_directory) = req.working_directory {
            workdir_path.join(working_directory)
        } else {
            workdir_path.to_owned()
        };
        let mut command = Command::new(&req.argv[0]);
        command
            .env_clear()
            // It would be really nice not to have to manually set PATH but this is sadly the only way
            // to stop automatic PATH searching.
            .env("PATH", "")
            .args(&req.argv[1..])
            .current_dir(cwd)
            .envs(&req.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = spawn_process(self.spawn_lock.clone(), exclusive_spawn, move || {
            ManagedChild::spawn(&mut command, None)
        })
        .await?;

        debug!("spawned local process as {:?} for {:?}", child.id(), req);
        let stdout_stream = FramedRead::new(child.stdout.take().unwrap(), BytesCodec::new())
            .map_ok(|bytes| ChildOutput::Stdout(bytes.into()))
            .fuse()
            .boxed();
        let stderr_stream = FramedRead::new(child.stderr.take().unwrap(), BytesCodec::new())
            .map_ok(|bytes| ChildOutput::Stderr(bytes.into()))
            .fuse()
            .boxed();
        let exit_stream = async move {
            child
                .wait()
                .map_ok(|exit_status| {
                    ChildOutput::Exit(ExitCode(
                        exit_status
                            .code()
                            .or_else(|| exit_status.signal().map(Neg::neg))
                            .expect("Child process should exit via returned code or signal."),
                    ))
                })
                .await
        }
        .into_stream()
        .boxed();
        let result_stream =
            futures::stream::select_all(vec![stdout_stream, stderr_stream, exit_stream]);

        Ok(result_stream
            .map_err(|e| format!("Failed to consume process outputs: {e:?}"))
            .boxed())
    }
}

/// Variations of errors that can occur when setting up the work directory for process execution.
#[derive(Debug)]
pub enum CapturedWorkdirError {
    Timeout {
        timeout: std::time::Duration,
        description: String,
    },
    Retryable(String),
    Fatal(String),
}

impl From<String> for CapturedWorkdirError {
    fn from(value: String) -> Self {
        Self::Fatal(value)
    }
}

impl Display for CapturedWorkdirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout {
                timeout,
                description,
            } => {
                write!(
                    f,
                    "Exceeded timeout of {:.1} seconds when executing local process: {}",
                    timeout.as_secs_f32(),
                    description
                )
            }
            Self::Retryable(message) => {
                write!(f, "{message} (retryable error)")
            }
            Self::Fatal(s) => write!(f, "{s}"),
        }
    }
}

#[async_trait]
pub trait CapturedWorkdir {
    type WorkdirToken: Clone + Send;

    fn apply_working_directory_to_outputs() -> bool {
        true
    }

    async fn run_and_capture_workdir(
        &self,
        req: Process,
        context: Context,
        store: Store,
        executor: Executor,
        workdir_path: PathBuf,
        workdir_token: Self::WorkdirToken,
        exclusive_spawn: bool,
    ) -> Result<FallibleProcessResultWithPlatform, CapturedWorkdirError> {
        let start_time = Instant::now();
        let mut stdout = BytesMut::with_capacity(8192);
        let mut stderr = BytesMut::with_capacity(8192);

        // Spawn the process.
        // NB: We fully buffer the `Stream` into the stdout/stderr buffers, but the idea going forward
        // is that we eventually want to pass incremental results on down the line for streaming
        // process results to console logs, etc.
        let exit_code_result = {
            let workdir_token = workdir_token.clone();
            let exit_code_future = collect_child_outputs(
                &mut stdout,
                &mut stderr,
                self.run_in_workdir(
                    &context,
                    &workdir_path,
                    workdir_token,
                    req.clone(),
                    exclusive_spawn,
                )
                .await?,
            );

            if let Some(req_timeout) = req.timeout {
                match timeout(req_timeout, exit_code_future).await {
                    Ok(Ok(exit_code)) => Ok(exit_code),
                    _ => Err(CapturedWorkdirError::Timeout {
                        timeout: req_timeout,
                        description: req.description.clone(),
                    }),
                }
            } else {
                exit_code_future.await.map_err(CapturedWorkdirError::from)
            }
        };

        // Capture the process outputs.
        self.prepare_workdir_for_capture(&context, &workdir_path, workdir_token, &req)
            .await?;
        let output_snapshot = if req.output_files.is_empty() && req.output_directories.is_empty() {
            store::Snapshot::empty()
        } else {
            let root = match (
                req.working_directory,
                Self::apply_working_directory_to_outputs(),
            ) {
                (Some(ref working_directory), true) => workdir_path.join(working_directory),
                _ => workdir_path.clone(),
            };
            // Use no ignore patterns, because we are looking for explicitly listed paths.
            let posix_fs = Arc::new(
                fs::PosixFS::new(root, fs::GitignoreStyleExcludes::empty(), executor.clone()).map_err(
                    |err| {
                        format!("Error making posix_fs to fetch local process execution output files: {err}")
                    },
                )?,
            );
            CommandRunner::construct_output_snapshot(
                store.clone(),
                posix_fs,
                req.output_files,
                req.output_directories,
            )
            .await?
        };

        let elapsed = start_time.elapsed();
        let result_metadata = ProcessResultMetadata::new(
            Some(elapsed.into()),
            ProcessResultSource::Ran,
            req.execution_environment,
            context.run_id,
        );

        let (exit_code, output_directory) = match exit_code_result {
            Ok(exit_code) => (exit_code, output_snapshot.into()),
            Err(timeout @ CapturedWorkdirError::Timeout { .. }) => {
                stderr.extend_from_slice(format!("\n\n{timeout}").as_bytes());
                (-libc::SIGTERM, EMPTY_DIRECTORY_DIGEST.clone())
            }
            Err(err) => return Err(err),
        };
        let (stdout_digest, stderr_digest) = try_join!(
            store.store_file_bytes(stdout.into(), true),
            store.store_file_bytes(stderr.into(), true),
        )?;
        Ok(FallibleProcessResultWithPlatform {
            stdout_digest,
            stderr_digest,
            exit_code,
            output_directory: output_directory,
            metadata: result_metadata,
        })
    }

    ///
    /// Spawn the given process in a working directory prepared with its expected input digest.
    ///
    /// NB: The implementer of this method must guarantee that the spawned process has completely
    /// exited when the returned BoxStream is Dropped. Otherwise it might be possible for the process
    /// to observe the working directory that it is running in being torn down. In most cases, this
    /// requires Drop handlers to synchronously wait for their child processes to exit.
    ///
    /// If the process to be executed has an `argv[0]` that points into its input digest then
    /// `exclusive_spawn` will be `true` and the spawn implementation should account for the
    /// possibility of concurrent fork+exec holding open the cloned `argv[0]` file descriptor, which,
    /// if unhandled, will result in ETXTBSY errors spawning the process.
    ///
    /// See the documentation note in `CommandRunner` in this file for more details.
    ///
    /// TODO(John Sirois): <https://github.com/pantsbuild/pants/issues/10601>
    ///  Centralize local spawning to one object - we currently spawn here (in
    ///  process_execution::local::CommandRunner) to launch user `Process`es and in
    ///  process_execution::nailgun::CommandRunner when a jvm nailgun server needs to be started. The
    ///  proper handling of `exclusive_spawn` really requires a single point of control for all
    ///  fork+execs in the scheduler. For now we rely on the fact that the process_execution::nailgun
    ///  module is dead code in practice.
    ///
    async fn run_in_workdir<'s, 'c, 'w, 'r>(
        &'s self,
        context: &'c Context,
        workdir_path: &'w Path,
        workdir_token: Self::WorkdirToken,
        req: Process,
        exclusive_spawn: bool,
    ) -> Result<BoxStream<'r, Result<ChildOutput, String>>, CapturedWorkdirError>;

    ///
    /// An optionally-implemented method which is called after the child process has completed, but
    /// before capturing the sandbox. The default implementation does nothing.
    ///
    async fn prepare_workdir_for_capture(
        &self,
        _context: &Context,
        _workdir_path: &Path,
        _workdir_token: Self::WorkdirToken,
        _req: &Process,
    ) -> Result<(), CapturedWorkdirError> {
        Ok(())
    }
}

///
/// Mutates a Process, replacing any `{chroot}` placeholders with `chroot_path`.
///
pub fn apply_chroot(chroot_path: &str, req: &mut Process) {
    for value in req.env.values_mut() {
        if value.contains("{chroot}") {
            *value = value.replace("{chroot}", chroot_path);
        }
    }
    for value in &mut req.argv {
        if value.contains("{chroot}") {
            *value = value.replace("{chroot}", chroot_path);
        }
    }
}

/// Creates a Digest for the entire input sandbox contents of the given Process, including absolute
/// symlinks to immutable inputs, named caches, and JDKs (if configured).
pub async fn prepare_workdir_digest(
    req: &Process,
    input_digest: DirectoryDigest,
    store: &Store,
    named_caches: &NamedCaches,
    immutable_inputs: Option<&ImmutableInputs>,
    named_caches_prefix: Option<&Path>,
    immutable_inputs_prefix: Option<&Path>,
) -> Result<DirectoryDigest, CapturedWorkdirError> {
    let mut paths = Vec::new();

    // Symlinks for immutable inputs and named caches.
    let mut workdir_symlinks = Vec::new();
    {
        if let Some(immutable_inputs) = immutable_inputs {
            let symlinks = immutable_inputs
                .local_paths(&req.input_digests.immutable_inputs)
                .await
                .map_err(|se| {
                    CapturedWorkdirError::Fatal(
                        se.enrich("An error occurred when creating symlinks to immutable inputs")
                            .to_string(),
                    )
                })?;

            match immutable_inputs_prefix {
                Some(prefix) => workdir_symlinks.extend(symlinks.into_iter().map(|symlink| {
                    WorkdirSymlink {
                        src: symlink.src,
                        dst: prefix.join(
                            symlink
                                .dst
                                .strip_prefix(immutable_inputs.workdir())
                                .unwrap(),
                        ),
                    }
                })),
                None => workdir_symlinks.extend(symlinks),
            }
        }

        let symlinks = named_caches.paths(&req.append_only_caches).await?;
        match named_caches_prefix {
            Some(prefix) => {
                workdir_symlinks.extend(symlinks.into_iter().map(|symlink| WorkdirSymlink {
                    src: symlink.src,
                    dst: prefix.join(symlink.dst.strip_prefix(named_caches.base_path()).unwrap()),
                }))
            }
            None => workdir_symlinks.extend(symlinks),
        }
    }
    paths.extend(workdir_symlinks.iter().map(|symlink| TypedPath::Link {
        path: &symlink.src,
        target: &symlink.dst,
    }));

    // Symlink for JDK.
    if let Some(jdk_home) = &req.jdk_home {
        paths.push(TypedPath::Link {
            path: Path::new(".jdk"),
            target: jdk_home,
        });
    }

    // The bazel remote execution API specifies that the parent directories for output files and
    // output directories should be created before execution completes.
    let parent_paths_to_create: HashSet<_> = req
        .output_files
        .iter()
        .chain(req.output_directories.iter())
        .filter_map(|rel_path| rel_path.as_ref().parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .collect();
    paths.extend(parent_paths_to_create.into_iter().map(TypedPath::Dir));

    // Finally, create a tree for all of the additional paths, and merge it with the input
    // Digest.
    let additions = DigestTrie::from_unique_paths(paths, &HashMap::new())?;

    store
        .merge(vec![input_digest, additions.into()])
        .await
        .map_err(|se| {
            CapturedWorkdirError::Fatal(
                se.enrich("An error occurred when merging digests")
                    .to_string(),
            )
        })
}

/// Prepares the given workdir for use by the given Process.
///
/// Returns true if the executable for the Process was created in the workdir, indicating that
/// `exclusive_spawn` is required.
///
pub async fn prepare_workdir(
    workdir_path: PathBuf,
    workdir_root_path: &Path,
    req: &Process,
    materialized_input_digest: DirectoryDigest,
    store: &Store,
    sandboxer: Option<&Sandboxer>,
    named_caches: &NamedCaches,
    immutable_inputs: &ImmutableInputs,
    named_caches_prefix: Option<&Path>,
    immutable_inputs_prefix: Option<&Path>,
) -> Result<bool, CapturedWorkdirError> {
    // Capture argv0 as the executable path so that we can test whether we have created it in the
    // sandbox.
    let maybe_executable_path = {
        let mut executable_path = PathBuf::from(&req.argv[0]);
        if executable_path.is_relative() {
            if let Some(working_directory) = &req.working_directory {
                executable_path = working_directory.as_ref().join(executable_path)
            }
            Some(workdir_path.join(executable_path))
        } else {
            None
        }
    };

    // Prepare the digest to use, and then materialize it.
    in_workunit!("setup_sandbox", Level::Debug, |_workunit| async move {
        let complete_input_digest = prepare_workdir_digest(
            req,
            materialized_input_digest,
            store,
            named_caches,
            Some(immutable_inputs),
            named_caches_prefix,
            immutable_inputs_prefix,
        )
        .await?;

        let mut mutable_paths = req.output_files.clone();
        mutable_paths.extend(req.output_directories.clone());

        if let Some(sandboxer) = sandboxer {
            debug!(
                "Materializing via sandboxer to {:?}: {:#?}",
                &workdir_path, &complete_input_digest
            );
            // Ensure that the tree is persisted in the store, so that the sandboxer
            // can materialize it from there.  Since record_digest_trie() takes ownership of its
            // argument, and we only need the digest anyway, we decompose the trie and digest
            // out of complete_input_digest.
            let persisted_digest =
                DirectoryDigest::from_persisted_digest(complete_input_digest.as_digest());
            if let Some(digest_trie) = complete_input_digest.tree {
                store
                    .record_digest_trie(digest_trie, true)
                    .await?;
            }
            sandboxer
                .materialize_directory(
                    &workdir_path,
                    workdir_root_path,
                    &persisted_digest,
                    &mutable_paths,
                )
                .await
                .map_err(|e| {
                    format!(
                        "materialize_directory() request to sandboxer process failed: {e}"
                    )
                })?;
        } else {
            debug!(
                "Materializing directly to {:?}: {:#?}",
                &workdir_path, &complete_input_digest
            );
            store
                .materialize_directory(
                    workdir_path.clone(),
                    workdir_root_path,
                    complete_input_digest,
                    false,
                    &mutable_paths,
                    Permissions::Writable,
                )
                .await
                .map_err(|se| se.enrich(format!("An error occurred when attempting to materialize a working directory at {workdir_path:#?}").as_str()).to_string())?;
        }

        if let Some(executable_path) = maybe_executable_path {
            Ok(tokio::fs::metadata(executable_path).await.is_ok())
        } else {
            Ok(false)
        }
    })
    .await
}

///
/// Creates an optionally-cleaned-up sandbox in the given base path.
///
/// If KeepSandboxes::Always, it is immediately marked preserved: otherwise, the caller should
/// decide whether to preserve it.
///
pub fn create_sandbox(
    executor: Executor,
    base_directory: &Path,
    description: &str,
    keep_sandboxes: KeepSandboxes,
) -> Result<AsyncDropSandbox, String> {
    let workdir = tempfile::Builder::new()
        .prefix("pants-sandbox-")
        .tempdir_in(base_directory)
        .map_err(|err| format!("Error making tempdir for local process execution: {err:?}"))?;

    let mut sandbox = AsyncDropSandbox(executor, workdir.path().to_owned(), Some(workdir));
    if keep_sandboxes == KeepSandboxes::Always {
        sandbox.keep(description);
    }
    Ok(sandbox)
}

/// Dropping sandboxes can involve a lot of IO, so it is spawned to the background as a blocking
/// task.
#[must_use]
pub struct AsyncDropSandbox(Executor, PathBuf, Option<TempDir>);

impl AsyncDropSandbox {
    pub fn path(&self) -> &Path {
        &self.1
    }

    ///
    /// Consume the `TempDir` without deleting directory on the filesystem, meaning that the
    /// temporary directory will no longer be automatically deleted when dropped.
    ///
    pub fn keep(&mut self, description: &str) {
        if let Some(workdir) = self.2.take() {
            let preserved_path = workdir.keep();
            info!(
                "Preserving local process execution dir {} for {}",
                preserved_path.display(),
                description,
            );
        }
    }
}

impl Drop for AsyncDropSandbox {
    fn drop(&mut self) {
        if let Some(sandbox) = self.2.take() {
            let _background_cleanup = self.0.native_spawn_blocking(|| std::mem::drop(sandbox));
        }
    }
}

/// Create a file called __run.sh with the env, cwd and argv used by Pants to facilitate debugging.
pub fn setup_run_sh_script(
    sandbox_path: &Path,
    env: &BTreeMap<String, String>,
    working_directory: &Option<RelativePath>,
    argv: &[String],
    workdir_path: &Path,
) -> Result<(), String> {
    let mut env_var_strings: Vec<String> = vec![];
    for (key, value) in env.iter() {
        let quoted_arg = Bash::quote_vec(value.as_str());
        let arg_str = str::from_utf8(&quoted_arg)
            .map_err(|e| format!("{e:?}"))?
            .to_string();
        let formatted_assignment = format!("{key}={arg_str}");
        env_var_strings.push(formatted_assignment);
    }
    let stringified_env_vars: String = env_var_strings.join(" ");

    // Shell-quote every command-line argument, as necessary.
    let mut full_command_line: Vec<String> = vec![];
    for arg in argv.iter() {
        let quoted_arg = Bash::quote_vec(arg.as_str());
        let arg_str = str::from_utf8(&quoted_arg)
            .map_err(|e| format!("{e:?}"))?
            .to_string();
        full_command_line.push(arg_str);
    }

    let stringified_cwd = {
        let cwd = if let Some(ref working_directory) = working_directory {
            workdir_path.join(working_directory)
        } else {
            workdir_path.to_owned()
        };
        let quoted_cwd = Bash::quote_vec(cwd.as_os_str());
        str::from_utf8(&quoted_cwd)
            .map_err(|e| format!("{e:?}"))?
            .to_string()
    };

    let stringified_command_line: String = full_command_line.join(" ");
    let full_script = format!(
        "#!/usr/bin/env bash
# This command line should execute the same process as pants did internally.
cd {stringified_cwd}
env -i {stringified_env_vars} {stringified_command_line}
",
    );

    let full_file_path = sandbox_path.join("__run.sh");

    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(USER_EXECUTABLE_MODE) // Executable for user, read-only for others.
        .open(full_file_path)
        .map_err(|e| format!("{e:?}"))?
        .write_all(full_script.as_bytes())
        .map_err(|e| format!("{e:?}"))
}
