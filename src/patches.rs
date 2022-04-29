use crate::PROGRESS_FLAG;
use camino::Utf8PathBuf;
use log::error;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use tempfile::NamedTempFile;
use toml_edit::{Document, InlineTable};

pub fn locate_workspace_folder(mut crate_path: PathBuf) -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or("cargo".to_owned());
    log::debug!("Checking workspace root of path {:?}", crate_path);
    crate_path.push("Cargo.toml");
    let output = Command::new(cargo)
        .arg("locate-project")
        .arg("--workspace")
        .arg("--manifest-path")
        .arg(crate_path.as_os_str().clone())
        .output()
        .expect("jojo");

    if !output.status.success() {
        return Err(format!("{:?}", output.status));
    }

    let output = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let parsed = json::parse(&output).map_err(|e| e.to_string())?;
    let root = parsed["root"].as_str().ok_or(String::from("no root"))?;
    let mut result = PathBuf::from(root);

    // Remove the trailing "/Cargo.toml"
    result.pop();
    Ok(result)
}

#[derive(Debug, Clone)]
pub struct PatchProject {
    pub name: OsString,
    pub local_path: PathBuf,
    pub remote_path: PathBuf,
}

impl PatchProject {
    pub fn new(name: OsString, path: PathBuf, remote_path: PathBuf) -> Self {
        PatchProject {
            name,
            local_path: path,
            remote_path,
        }
    }
}

fn extract_patched_crates_and_adjust_toml<F: Fn(PathBuf) -> Result<PathBuf, String>>(
    manifest_content: String,
    locate_workspace: F,
) -> Option<(Document, Vec<PatchProject>)> {
    let mut manifest = manifest_content.parse::<Document>().expect("invalid doc");
    let mut workspaces_to_copy: Vec<PatchProject> = Vec::new();

    // A list of inline tables like
    // { path = "/some/path" }
    let patched_paths: Option<Vec<&mut InlineTable>> =
        manifest["patch"].as_table_mut().map(|patch| {
            patch
                .iter_mut()
                .filter_map(|(_, crate_table)| crate_table.as_table_mut())
                .flat_map(|crate_table| {
                    crate_table
                        .iter_mut()
                        .filter_map(|(_, patch_table)| patch_table.as_inline_table_mut())
                })
                .collect()
        });

    if patched_paths.is_none() {
        log::debug!("No patches in project.");
        return None;
    }

    for inline_crate_table in patched_paths.unwrap() {
        // We only act if there is a path given for a crate
        if let Some(path) = inline_crate_table.get("path") {
            let path = PathBuf::from(path.as_str().unwrap().clone());

            // Check if the current crate is located in a subfolder of a workspace we
            // already know.
            let known_workspace = workspaces_to_copy
                .iter()
                .find(|known_target| path.starts_with(&known_target.local_path));
            match known_workspace {
                None => {
                    // Project is unknown and needs to be copied
                    let workspace_folder_path =
                        locate_workspace(path.clone()).expect("Can not determine workspace path");
                    let workspace_folder_name =
                        workspace_folder_path.file_name().unwrap().to_owned();

                    let mut remote_folder = PathBuf::from("../");
                    remote_folder.push(workspace_folder_name.clone());

                    log::debug!(
                        "Found referenced project '{:?}', will copy to '{:?}'",
                        &workspace_folder_path,
                        &remote_folder
                    );

                    // Add workspace to the list so it will be rsynced to the remote server
                    workspaces_to_copy.push(PatchProject::new(
                        workspace_folder_name,
                        workspace_folder_path.clone(),
                        remote_folder.clone(),
                    ));

                    // Build a new path for the crate relative to the workspace folder
                    remote_folder.push(path.strip_prefix(workspace_folder_path).expect("Jawoll"));
                    inline_crate_table.insert(
                        "path",
                        toml_edit::Value::from(remote_folder.to_str().unwrap()),
                    );
                }

                Some(patch_target) => {
                    let mut new_path = patch_target.remote_path.clone();
                    new_path.push(path.strip_prefix(&patch_target.local_path).expect("Jawoll"));
                    inline_crate_table
                        .insert("path", toml_edit::Value::from(new_path.to_str().unwrap()));
                }
            }
        }
    }
    Some((manifest, workspaces_to_copy))
}

/// Handle patched dependencies in a Cargo.toml file.
/// Adjustments are only needed when patches point to local files.
/// Steps:
/// 1. Read Cargo.toml of project
/// 2. Extract list of patches
/// 3. For each patched crate, check if there is a path given. If not, ignore.
/// 4. Find the workspace of the patched crate via `cargo locate-project --workspace`
/// 5. Add workspace to the list of projects that need to be copied
/// 6. Copy folders via rsync
pub fn handle_patches(
    build_path: &String,
    build_server: &String,
    manifest_path: Utf8PathBuf,
) -> Result<(), String> {
    let cargo_file_content = std::fs::read_to_string(manifest_path)
        .ok()
        .expect("Shold work");

    let maybe_patches =
        extract_patched_crates_and_adjust_toml(cargo_file_content, |p| locate_workspace_folder(p));

    if let Some((patched_cargo_doc, project_list)) = maybe_patches {
        let mut tmp_cargo_file = NamedTempFile::new().expect("No tempfile for us");
        tmp_cargo_file
            .write_all(patched_cargo_doc.to_string().as_bytes())
            .expect("Unable to write file");

        copy_patches_to_remote(&build_path, &build_server, tmp_cargo_file, project_list);
    }
    Ok(())
}

fn copy_patches_to_remote(
    build_path: &String,
    build_server: &String,
    patched_cargo_file: NamedTempFile,
    projects_to_copy: Vec<PatchProject>,
) {
    for patch_operation in projects_to_copy.iter() {
        let local_proj_path = format!("{}/", patch_operation.local_path.to_string_lossy());
        let remote_proj_path = format!(
            "{}:{}",
            build_server,
            patch_operation.remote_path.to_string_lossy()
        );
        log::debug!(
            "Copying workspace {:?} from {} to {}.",
            patch_operation.name,
            &local_proj_path,
            &remote_proj_path
        );
        // transfer project to build server
        let mut rsync_to = Command::new("rsync");
        rsync_to
            .arg("-a")
            .arg("-q")
            .arg("--delete")
            .arg("--compress")
            .arg(PROGRESS_FLAG)
            .arg("--exclude")
            .arg("target")
            .arg("--exclude")
            .arg(".*")
            .arg("--rsync-path")
            .arg("mkdir -p remote-builds/patches && rsync")
            .arg(local_proj_path)
            .arg(remote_proj_path)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!("Failed to transfer project to build server (error: {})", e);
                exit(-4);
            });
    }

    let local_toml_path = patched_cargo_file.path().to_string_lossy();
    let remote_toml_path = format!("{}:{}/Cargo.toml", build_server, build_path);
    log::debug!(
        "Transferring Cargo.toml from {} to {}.",
        &local_toml_path,
        &remote_toml_path
    );
    let mut rsync_toml = Command::new("rsync");
    rsync_toml
        .arg("-vz")
        .arg(PROGRESS_FLAG)
        .arg(local_toml_path.to_string())
        .arg(remote_toml_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to transfer project to build server (error: {})", e);
            exit(-4);
        });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::patches::extract_patched_crates_and_adjust_toml;

    #[test]
    fn simple_modification_replaces_path() {
        let input = r#"
"hello" = 'toml!'
[patch.a]
a-crate = { path = "/some/prefix/a/src/a-crate" }
a-other-crate = { path = "/some/prefix/a/src/subfolder/a-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
[patch.b]
b-crate = { path = "/some/prefix/b/src/b-crate" }
b-other-crate = { path = "/some/prefix/b/src/subfolder/b-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
"#
        .to_string();
        let expect = r#"
"hello" = 'toml!'
[patch.a]
a-crate = { path = "../a/src/a-crate" }
a-other-crate = { path = "../a/src/subfolder/a-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
[patch.b]
b-crate = { path = "../b/src/b-crate" }
b-other-crate = { path = "../b/src/subfolder/b-other-crate" }
git-patched-crate = { git = "https://some-url/test/test" }
"#
        .to_string();

        let result = extract_patched_crates_and_adjust_toml(input, |p| {
            if p.starts_with("/some/prefix/a") {
                return Ok(PathBuf::from("/some/prefix/a"));
            } else if p.starts_with("/some/prefix/b") {
                return Ok(PathBuf::from("/some/prefix/b"));
            }
            Err("Invalid Path".to_string())
        })
        .unwrap();
        assert_eq!(result.0.to_string(), expect);
    }
}
