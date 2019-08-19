use crate::dep_types::{Constraint, DependencyError, Lock, LockPackage, Req, ReqType, Version};
use crate::util::abort;
use crossterm::Color;
use install::PackageType::{Source, Wheel};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::HashMap,
    env,
    error::Error,
    fmt, fs,
    io::{self, BufRead, BufReader},
    path::PathBuf,
    process::Command,
    str::FromStr,
    thread, time,
};

use structopt::StructOpt;

mod build;
mod commands;
mod dep_resolution;
mod dep_types;
mod edit_files;
mod install;
mod util;

#[derive(Copy, Clone, Debug, Deserialize, PartialEq)]
/// Used to determine which version of a binary package to download. Assume 64-bit.
pub enum Os {
    Linux32,
    Linux,
    Windows32,
    Windows,
    //    Mac32,
    Mac,
    Any,
}

impl FromStr for Os {
    type Err = dep_types::DependencyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "manylinux1_i686" => Os::Linux32,
            "manylinux1_x86_64" => Os::Linux,
            "win32" => Os::Windows32,
            "win_amd64" => Os::Windows,
            "darwin" => Os::Mac,
            "any" => Os::Any,
            _ => {
                if s.contains("mac") {
                    Os::Mac
                } else {
                    return Err(DependencyError::new("Problem parsing Os"));
                }
            }
        })
    }
}

#[derive(StructOpt, Debug)]
#[structopt(name = "Pypackage", about = "Python packaging and publishing")]
//#[structopt(raw(setting = "structopt::clap::AppSettings:::AllowExternalSubcommands"))]
struct Opt {
    #[structopt(subcommand)]
    subcmds: Option<SubCommand>,
    #[structopt(name = "script")]
    //    #[structopt(raw(setting = "structopt::clap::AppSettings::TrailingVarArg"))]
    script: Vec<String>,
}

#[derive(StructOpt, Debug)]
enum SubCommand {
    /// Create a project folder with the basics
    #[structopt(name = "new")]
    New {
        #[structopt(name = "name")]
        name: String, // holds the project name.
    },

    /// Install packages from `pyproject.toml`, or ones specified
    #[structopt(
        name = "install",
        help = "
Install packages from `pyproject.toml`, `pypackage.lock`, or speficied ones. Example:

`pypackage install`: sync your installation with `pyproject.toml`, or `pypackage.lock` if it exists.
`pypackage install numpy scipy`: install `numpy` and `scipy`.
"
    )]
    Install {
        #[structopt(name = "packages")]
        packages: Vec<String>,
    },
    /// Uninstall all packages, or ones specified
    #[structopt(name = "uninstall")]
    Uninstall {
        #[structopt(name = "packages")]
        packages: Vec<String>,
    },
    /// Run python
    #[structopt(name = "python")]
    Python {
        #[structopt(name = "args")]
        args: Vec<String>,
    },
    /// Display all installed packages and console scripts
    #[structopt(name = "list")]
    List,
    /// Build the package - source and wheel
    #[structopt(name = "package")]
    Package {
        #[structopt(name = "extras")]
        extras: Vec<String>, // todo: rename features?
    },
    /// Publish to `pypi`
    #[structopt(name = "publish")]
    Publish,
    /// Create a `pyproject.toml` from requirements.txt, pipfile etc, setup.py etc
    #[structopt(name = "init")]
    Init,
    /// Remove the environment, and uninstall all packages
    #[structopt(name = "reset")]
    Reset,
    /// Run a CLI script like `ipython` or `black`. Note that you can simply run `pypackage black`
    /// as a shortcut.
    #[structopt(name = "run")] // We don't need to invoke this directly, but the option exists
    Run {
        #[structopt(name = "args")]
        args: Vec<String>,
    },
}

/// A config, parsed from pyproject.toml
#[derive(Clone, Debug, Default, Deserialize)]
// todo: Auto-desr some of these
pub struct Config {
    py_version: Option<Constraint>,
    reqs: Vec<Req>, // name, requirements.
    name: Option<String>,
    version: Option<Version>,
    author: Option<String>,
    author_email: Option<String>,
    license: Option<String>,
    extras: Option<HashMap<String, Vec<String>>>,
    description: Option<String>,
    classifiers: Vec<String>, // https://pypi.org/classifiers/
    keywords: Vec<String>,    // todo: Options for classifiers and keywords?
    homepage: Option<String>,
    repo_url: Option<String>,
    package_url: Option<String>,
    readme_filename: Option<String>,
    entry_points: HashMap<String, Vec<String>>, // todo option?
}

fn key_re(key: &str) -> Regex {
    Regex::new(&format!(r#"^{}\s*=\s*"(.*)"$"#, key)).unwrap()
}

impl Config {
    /// Pull config data from `pyproject.toml`
    fn from_file(filename: &str) -> Option<Self> {
        // We don't use the `toml` crate here because it doesn't appear flexible enough.
        let mut result = Config::default();
        let file = match fs::File::open(filename) {
            Ok(f) => f,
            Err(_) => return None,
        };

        let mut in_metadata = false;
        let mut in_dep = false;
        let mut in_extras = false;

        let sect_re = Regex::new(r"\[.*\]").unwrap();

        for line in BufReader::new(file).lines() {
            if let Ok(l) = line {
                // todo replace this with something that clips off
                // todo post-# part of strings; not just ignores ones starting with #
                if l.starts_with('#') {
                    continue;
                }

                if &l == "[tool.pypackage]" {
                    in_metadata = true;
                    in_dep = false;
                    in_extras = false;
                    continue;
                } else if &l == "[tool.pypackage.dependencies]" {
                    in_metadata = false;
                    in_dep = true;
                    in_extras = false;
                    continue;
                } else if &l == "[tool.pypackage.features]" {
                    in_metadata = false;
                    in_dep = false;
                    in_extras = true;
                    continue;
                } else if sect_re.is_match(&l) {
                    in_metadata = false;
                    in_dep = false;
                    in_extras = false;
                    continue;
                }

                if in_metadata {
                    // todo DRY
                    if let Some(n2) = key_re("name").captures(&l) {
                        if let Some(n) = n2.get(1) {
                            result.name = Some(n.as_str().to_string());
                        }
                    }
                    if let Some(n2) = key_re("description").captures(&l) {
                        if let Some(n) = n2.get(1) {
                            result.description = Some(n.as_str().to_string());
                        }
                    }
                    if let Some(n2) = key_re("version").captures(&l) {
                        if let Some(n) = n2.get(1) {
                            let n3 = n.as_str();
                            if !n3.is_empty() {
                                result.version = Some(Version::from_str(n3).unwrap());
                            }
                        }
                    }
                    if let Some(n2) = key_re("py_version").captures(&l) {
                        if let Some(n) = n2.get(1) {
                            let n3 = n.as_str();
                            if !n3.is_empty() {
                                result.py_version = Some(Constraint::from_str(n.as_str()).unwrap());
                            }
                        }
                    }
                } else if in_dep && !l.is_empty() {
                    result.reqs.push(Req::from_str(&l, false).unwrap());
                }
            }
        }

        Some(result)
    }

    /// Create a new `pyproject.toml` file.
    fn write_file(&self, filename: &str) {
        let file = PathBuf::from(filename);
        if file.exists() {
            abort("`pyproject.toml` already exists")
        }

        let mut result = String::new();

        result.push_str("[tool.pypackage]\n");
        if let Some(name) = &self.name {
            result.push_str(&("name = \"".to_owned() + name + "\"\n"));
        } else {
            // Give name, and a few other fields default values.
            result.push_str(&("name = \"\"".to_owned() + "\n"));
        }
        if let Some(py_v) = &self.py_version {
            result.push_str(&("version = \"".to_owned() + &py_v.to_string(false, false) + "\"\n"));
        } else {
            result.push_str(&("version = \"\"".to_owned() + "\n"));
        }
        if let Some(vers) = self.version {
            result.push_str(&(vers.to_string() + "\n"));
        }
        if let Some(author) = &self.author {
            result.push_str(&(author.to_owned() + "\n"));
        }

        result.push_str("\n\n");
        result.push_str("[tool.pypackage.dependencies]\n");
        for dep in self.reqs.iter() {
            result.push_str(&(dep.to_cfg_string() + "\n"));
        }

        match fs::write(file, result) {
            Ok(_) => util::print_color("Created `pyproject.toml`", Color::Green),
            Err(_) => abort("Problem writing `pyproject.toml`"),
        }
    }
}

/// Create a template directory for a python project.
pub(crate) fn new(name: &str) -> Result<(), Box<dyn Error>> {
    if !PathBuf::from(name).exists() {
        fs::create_dir_all(&format!("{}/{}", name, name))?;
        fs::File::create(&format!("{}/{}/main.py", name, name))?;
        fs::File::create(&format!("{}/README.md", name))?;
        fs::File::create(&format!("{}/LICENSE", name))?;
        fs::File::create(&format!("{}/pyproject.toml", name))?;
        fs::File::create(&format!("{}/.gitignore", name))?;
    }

    let gitignore_init = r##"# General Python ignores

build/
dist/
__pycache__/
.ipynb_checkpoints/
*.pyc
*~
*/.mypy_cache/


# Project ignores
"##;

    let pyproject_init = &format!(
        r##"[tool.pypackage]
name = "{}"
py_version = "^3.7"
version = "0.1.0"
description = ""
author = ""

pyackage_url = "https://test.pypi.org"
# pyackage_url = "https://pypi.org"

[tool.pypackage.dependencies]
"##,
        name
    );

    // todo: flesh readme out
    let readme_init = &format!("# {}", name);

    fs::write(&format!("{}/.gitignore", name), gitignore_init)?;
    fs::write(&format!("{}/pyproject.toml", name), pyproject_init)?;
    fs::write(&format!("{}/README.md", name), readme_init)?;

    Ok(())
}

/// Prompt which Python alias to use, if multiple are found.
fn prompt_alias(aliases: &[(String, Version)]) -> (String, Version) {
    // Todo: Overall, the API here is inelegant.
    println!("Found multiple Python aliases. Please enter the number associated with the one you'd like to use for this project:");
    for (i, (alias, version)) in aliases.iter().enumerate() {
        println!("{}: {} version: {}", i + 1, alias, version.to_string())
    }

    let mut mapping = HashMap::new();
    for (i, alias) in aliases.iter().enumerate() {
        mapping.insert(i + 1, alias);
    }

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .expect("Unable to read user input for version");

    let input = input
        .chars()
        .next()
        .expect("Problem reading input")
        .to_string();

    let (alias, version) = mapping
        .get(
            &input
                .parse::<usize>()
                .expect("Enter the number associated with the Python alias."),
        )
        .expect(
            "Can't find the Python alias associated with that number. Is it in the list above?",
        );
    (alias.to_string(), *version)
}

#[derive(Debug)]
pub struct AliasError {
    details: String,
}

impl Error for AliasError {
    fn description(&self) -> &str {
        &self.details
    }
}

impl fmt::Display for AliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

/// Make an educated guess at the command needed to execute python the
/// current system.  An alternative approach is trying to find python
/// installations.
fn find_py_alias() -> Result<(String, Version), AliasError> {
    let possible_aliases = &[
        "python3.10",
        "python3.9",
        "python3.8",
        "python3.7",
        "python3.6",
        "python3.5",
        "python3.4",
        "python3.3",
        "python3.2",
        "python3.1",
        "python3",
        "python",
        "python2",
    ];

    let mut found_aliases = Vec::new();

    for alias in possible_aliases {
        // We use the --version command as a quick+effective way to determine if
        // this command is associated with Python.
        if let Some(v) = commands::find_py_version(alias) {
            found_aliases.push((alias.to_string(), v));
        }
    }

    match found_aliases.len() {
        0 => Err(AliasError {
            details: "Can't find Python on the path.".into(),
        }),
        1 => Ok(found_aliases[0].clone()),
        _ => Ok(prompt_alias(&found_aliases)),
    }
}

/// Read dependency data from a lock file.
fn read_lock(filename: &str) -> Result<(Lock), Box<dyn Error>> {
    let data = fs::read_to_string(filename)?;
    //    let t: Lock = toml::from_str(&data).unwrap();
    Ok(toml::from_str(&data)?)
}

/// Write dependency data to a lock file.
fn write_lock(filename: &str, data: &Lock) -> Result<(), Box<dyn Error>> {
    let data = toml::to_string(data)?;
    fs::write(filename, data)?;
    Ok(())
}

/// Find the operating system from a wheel filename. This doesn't appear to be available
/// anywhere else on the Pypi Warehouse.
fn os_from_wheel_fname(filename: &str) -> Result<(Os), dep_types::DependencyError> {
    // Format is "name-version-pythonversion-mobileversion?-os.whl"
    // Also works with formats like this:
    // `PyQt5-5.13.0-5.13.0-cp35.cp36.cp37.cp38-none-win32.whl` too.
    // The point is, pull the last part before ".whl".
    let re = Regex::new(r"^(?:.*?-)+(.*).whl$").unwrap();
    if let Some(caps) = re.captures(filename) {
        let parsed = caps.get(1).unwrap().as_str();
        return Ok(Os::from_str(parsed).expect(&format!("Problem parsing Os: {}", parsed)));
    }

    Err(dep_types::DependencyError::new(
        "Problem parsing os from wheel name",
    ))
}

/// Create a new virtual environment, and install Wheel.
fn create_venv(cfg_v: Option<&Constraint>, pyypackages_dir: &PathBuf) -> Version {
    // We only use the alias for creating the virtual environment. After that,
    // we call our venv's executable directly.

    // todo perhaps move alias finding back into create_venv, or make a
    // todo create_venv_if_doesnt_exist fn.
    let (alias, py_ver_from_alias) = match find_py_alias() {
        Ok(a) => a,
        Err(_) => {
            abort("Unable to find a Python version on the path");
            ("".to_string(), Version::new_short(0, 0)) // Required for compiler
        }
    };

    let vers_path = pyypackages_dir.join(format!(
        "{}.{}",
        py_ver_from_alias.major, py_ver_from_alias.minor
    ));

    let lib_path = vers_path.join("lib");

    if !lib_path.exists() {
        fs::create_dir_all(&lib_path).expect("Problem creating __pypackages__ directory");
    }

    if let Some(c_v) = cfg_v {
        // We don't expect the config version to specify a patch, but if it does, take it
        // into account.
        if !c_v.is_compatible(&py_ver_from_alias) {
            abort(&format!("The Python version you selected ({}) doesn't match the one specified in `pyprojecttoml` ({})",
                           py_ver_from_alias.to_string(), c_v.to_string(false, false))
            );
        }
    }

    println!("Setting up Python environment...");

    if commands::create_venv(&alias, &lib_path, ".venv").is_err() {
        util::abort("Problem creating virtual environment");
    }

    //    let bin_path = vers_path.join(".venv/bin"); // todo
    //
    //    // Wait until the venv's created before continuing, or we'll get errors
    //    // when attempting to use it
    //    let py_venv = bin_path.join("python");
    //    let pip_venv = bin_path.join("pip");
    //    util::wait_for_dirs(&[py_venv, pip_venv]).unwrap();
    //
    //    // todo: Not sure why we need this extra sleep. Wheel won't install if
    //    // todo we don't have it.
    //    thread::sleep(time::Duration::from_millis(200));

    // todo: Chicken-egg scenario where we need to wait for the venv to complete before
    // todo installing `wheel` and returning, but don't know what folder
    // todo to look for in wait_for_dirs. Blanket sleep for now.
    thread::sleep(time::Duration::from_millis(2000));
    let bin_path = util::find_bin_path(&vers_path);

    // We need `wheel` installed to build wheels from source.
    // Note: This installs to the venv's site-packages, not __pypackages__/3.x/lib.
    Command::new("./python")
        .current_dir(bin_path)
        .args(&["-m", "pip", "install", "--quiet", "wheel"])
        .spawn()
        .expect("Problem installing `wheel`");

    py_ver_from_alias
}

/// Install/uninstall deps as required from the passed list, and re-write the lock file.
fn sync_deps(
    lock_filename: &str,
    bin_path: &PathBuf,
    lib_path: &PathBuf,
    reqs: &[Req],
    installed: &[(String, Version)],
    python_vers: &Version,
) {
    #[cfg(target_os = "windows")]
    let os = Os::Windows;
    #[cfg(target_os = "linux")]
    let os = Os::Linux;
    #[cfg(target_os = "macos")]
    let os = Os::Mac;

    println!("Resolving dependencies...");

    //    // Recursively add sub-dependencies.
    //    let mut tree = DepNode {
    //        // dummy parent
    //        name: String::from("root"),
    //        version: Version::new(0, 0, 0),
    //        reqs: reqs.to_vec(), // todo clone?
    //        dependencies: vec![],
    //        extras: vec![],
    //        constraints_for_this: vec![],
    //    };

    println!("REQS: {:?}", &reqs);
    let extras = vec![];

    let resolved = match dep_resolution::resolve(reqs, installed, &os, &extras, python_vers) {
        //    let resolved = match dep_resolution::resolve(&mut tree) {
        Ok(r) => r,
        Err(_) => {
            abort("Problem resolving dependencies");
            vec![] // todo find proper way to equlaize mathc arms.
        }
    };

    // Resolve is made from non-nested deps, with their subdeps stripped: It's flattened.
    for (name, version) in resolved.iter() {
        // Move on if we've already installed this specific package/version
        let mut already_installed = false;
        for (inst_name, inst_ver) in installed.iter() {
            if inst_name.to_lowercase() == name.to_lowercase() && inst_ver == version {
                already_installed = true;
            }
        }
        if already_installed {
            continue;
        }

        let data = dep_resolution::get_warehouse_release(&name, &version)
            .expect("Problem getting warehouse data");

        let mut compatible_releases = vec![];
        // Store source releases as a fallback, for if no wheels are found.
        let mut source_releases = vec![];

        for rel in data.iter() {
            let mut compatible = true;
            match rel.packagetype.as_ref() {
                "bdist_wheel" => {
                    if let Some(py_ver) = &rel.requires_python {
                        // If a version constraint exists, make sure it's compatible.
                        let py_req = Constraint::from_str(&py_ver)
                            .expect("Problem parsing constraint from requires_python");

                        if !py_req.is_compatible(&python_vers) {
                            compatible = false;
                        }
                    }

                    let wheel_os = os_from_wheel_fname(&rel.filename)
                        .expect("Problem getting os from wheel name");
                    if wheel_os != os && wheel_os != Os::Any {
                        compatible = false;
                    }

                    // Packages that use C code(eg numpy) may fail to load C extensions if installing
                    // for the wrong version of python (eg  cp35 when python 3.7 is installed), even
                    // if `requires_python` doesn't indicate an incompatibility. Check `python_version`.
                    match Version::from_cp_str(&rel.python_version) {
                        Ok(req_v) => {
                            if req_v != *python_vers
                                // todo: Awk place for this logic.
                                && rel.python_version != "py2.py3"
                                && rel.python_version != "py3"
                            {
                                compatible = false;
                            }
                        }
                        Err(_) => {
                            (println!(
                                "Unable to match python version from python_version: {}",
                                &rel.python_version
                            ))
                        } // todo
                    }

                    if compatible {
                        compatible_releases.push(rel.clone());
                    }
                }
                "sdist" => source_releases.push(rel.clone()),
                _ => abort(&format!(
                    "Found surprising package type: {}",
                    rel.packagetype
                )),
            }
        }

        let best_release;
        let package_type;
        // todo: Sort further / try to match exact python_version if able.
        if compatible_releases.is_empty() {
            if source_releases.is_empty() {
                abort(&format!(
                    "Unable to find a compatible release for {}: {}",
                    name,
                    version.to_string()
                ));
                best_release = &compatible_releases[0]; // todo temp
                package_type = Wheel // todo temp to satisfy match
            } else {
                best_release = &source_releases[0];
                package_type = Source;
            }
        } else {
            best_release = &compatible_releases[0];
            package_type = Wheel;
        }

        println!(
            "Downloading and installing {} = \"{}\"",
            &name,
            &version.to_string()
        );

        if install::download_and_install_package(
            &name,
            &version,
            &best_release.url,
            &best_release.filename,
            &best_release.digests.sha256,
            lib_path,
            bin_path,
            package_type,
        )
        .is_err()
        {
            abort("Problem downloading packages");
        }
    }

    for (inst_name, inst_vers) in installed.iter() {
        let mut required = false;
        for (name, version) in resolved.iter() {
            if name.to_lowercase() == inst_name.to_lowercase() && version == inst_vers {
                required = true;
            }
        }
        if !required {
            install::uninstall(inst_name, inst_vers, lib_path)
        }
    }

    //    let lock_metadata = resolved.iter().map(|dep|
    //        // todo: Probably incorporate hash etc info in the depNode.
    //        format!("\"checksum {} {} ({})\" = \"{}\"", &dep.name, &dep.version.to_string(), "", "placeholder")
    //    )
    //        .collect();

    let lock_packs = resolved
        .into_iter()
        .map(|(name, version)| LockPackage {
            name: name.clone(),
            version: version.to_string(),
            source: Some(format!(
                "pypi+https://pypi.org/pypi/{}/{}/json",
                name,
                version.to_string()
            )), // todo
            dependencies: None, // todo!
        })
        .collect();

    let new_lock = Lock {
        //        metadata: Some(lock_metadata),
        metadata: None, // todo: Problem with toml conversion.
        package: Some(lock_packs),
    };

    if write_lock(lock_filename, &new_lock).is_err() {
        abort("Problem writing lock file");
    }
}

fn main() {
    // todo perhaps much of this setup code should only be in certain match branches.
    let cfg_filename = "pyproject.toml";
    let lock_filename = "pypackage.lock";

    let mut cfg = Config::from_file(cfg_filename).unwrap_or_default();

    let opt = Opt::from_args();
    let subcmd = match opt.subcmds {
        Some(sc) => sc,
        None => SubCommand::Run { args: opt.script },
    };

    // New doesn't execute any other logic. Init must execute befor the rest of the logic,
    // since it sets up a new (or modified) `pyproject.toml`. The rest of the commands rely
    // on the virtualenv and `pyproject.toml`, so make sure those are set up before processing them.
    match subcmd {
        SubCommand::New { name } => {
            new(&name).expect("Problem creating project");
            util::print_color(
                &format!("Created a new Python project named {}", name),
                Color::Green,
            );
            return;
        }
        SubCommand::Init {} => {
            edit_files::parse_req_dot_text(&mut cfg);
            edit_files::parse_pipfile(&mut cfg);
            edit_files::parse_poetry(&mut cfg);

            cfg.write_file(cfg_filename);
        }
        _ => (),
    }

    let pypackages_dir = env::current_dir()
        .expect("Can't find current path")
        .join("__pypackages__");

    let py_version_cfg = cfg.py_version.clone();

    // Check for environments. Create one if none exist. Set `vers_path`.
    let mut vers_path = PathBuf::new();
    let mut py_vers = Version::new(0, 0, 0);

    match py_version_cfg {
        // The version's explicitly specified; check if an environment for that version
        // exists. If not, create one, and make sure it's the right version.
        Some(cfg_v) => {
            // The version's specified in the config. Ensure a virtualenv for this
            // is setup.  // todo: Confirm using --version on the python bin, instead of relying on folder name.

            if !util::venv_exists(&pypackages_dir.join(&format!(
                "{}.{}/.venv",
                cfg_v.major,
                cfg_v.minor.unwrap_or(0)
            ))) {
                create_venv(None, &pypackages_dir);
            }

            // Don't include version patch in the directory name, per PEP 582.
            vers_path =
                pypackages_dir.join(&format!("{}.{}", cfg_v.major, cfg_v.minor.unwrap_or(0)));

            // todo: Take into account type of version! Currently ignores, and just takes the major/minor,
            // todo, but we're dealing with a constraint.
            py_vers = cfg_v.version();
        }
        // The version's not specified in the config; Search for existing environments, and create
        // one if we can't find any.
        None => {
            // Note that we rely on the proper folder name, vice inspecting the binary.
            // ie: could also check `bin/python --version`.
            let venv_versions_found: Vec<Version> = util::possible_py_versions()
                .into_iter()
                .filter(|v| {
                    util::venv_exists(
                        &pypackages_dir.join(&format!("{}.{}/.venv", v.major, v.minor)),
                    )
                })
                .collect();

            match venv_versions_found.len() {
                0 => {
                    let created_vers = create_venv(None, &pypackages_dir);
                    vers_path = pypackages_dir
                        .join(&format!("{}.{}", created_vers.major, created_vers.minor));
                    py_vers = Version::new_short(created_vers.major, created_vers.minor);
                }
                1 => {
                    vers_path = pypackages_dir.join(&format!(
                        "{}.{}",
                        venv_versions_found[0].major, venv_versions_found[0].minor
                    ));
                    py_vers = Version::new_short(
                        venv_versions_found[0].major,
                        venv_versions_found[0].minor,
                    );
                }
                _ => abort(
                    "Multiple Python environments found
                for this project; specify the desired one in `pyproject.toml`. Example:
[tool.pypackage]
py_version = \"3.7\"",
                ),
            }
        }
    };

    let lib_path = vers_path.join("lib");
    let bin_path = util::find_bin_path(&vers_path);

    let lock = match read_lock(lock_filename) {
        Ok(l) => {
            println!("Found lockfile");
            l
        }
        Err(_) => Lock::default(),
    };

    match subcmd {
        // Add pacakge names to `pyproject.toml` if needed. Then sync installed packages
        // and `pyproject.lock` with the `pyproject.toml`.
        SubCommand::Install { packages } => {
            let mut added_reqs = vec![];
            for p in packages.into_iter() {
                match Req::from_str(&p, false) {
                    Ok(r) => added_reqs.push(r),
                    Err(_) => abort(&format!("Unable to parse this package: {}. \
                    Note that installing a specific version via the CLI is currently unsupported. If you need to specify a version,\
                     edit `pyproject.toml`", &p)),
                }
            }

            // Reqs to add to `pyproject.toml`
            let mut added_reqs_unique: Vec<Req> = added_reqs
                .into_iter()
                .filter(|ar| {
                    // return true if the added req's not in the cfg reqs, or if it is
                    // and the version's different.
                    let mut add = true;
                    for cr in cfg.reqs.iter() {
                        if cr == ar
                            || (cr.name.to_lowercase() == ar.name.to_lowercase()
                                && ar.constraints.is_empty())
                        {
                            // Same req/version exists
                            add = false;
                            break;
                        }
                    }
                    add
                })
                .collect();

            // If no constraints are specified, use a carot constraint with the latest
            // version.
            for added_req in added_reqs_unique.iter_mut() {
                if added_req.constraints.is_empty() {
                    let (formatted_name, vers, _) =
                        dep_resolution::get_version_info(&added_req.name)
                            .expect("Problem getting latest version of the package you added.");
                    added_req.constraints.push(Constraint::new(
                        ReqType::Caret,
                        vers.major,
                        vers.minor,
                        vers.patch,
                    ));
                }
            }

            let mut merged_reqs = vec![]; // Reqs to sync

            // Merge reqs from the config and added via CLI. If there's a conflict in version,
            // use the added req.
            for cr in cfg.reqs.into_iter() {
                let mut replaced = false;
                for added_req in added_reqs_unique.iter() {
                    if added_req.name == cr.name && added_req.constraints != cr.constraints {
                        merged_reqs.push(added_req.clone());
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    merged_reqs.push(cr);
                }
            }

            if !added_reqs_unique.is_empty() {
                edit_files::add_reqs_to_cfg(cfg_filename, &added_reqs_unique);
            }

            merged_reqs.append(&mut added_reqs_unique);

            let installed = util::find_installed(&lib_path);

            // todo excessive nesting
            // If able, tie reqs to a specific version specified in the lock.
            if let Some(lock_packs) = lock.package {
                for req in merged_reqs.iter_mut() {
                    for lock_pack in lock_packs.iter() {
                        let lock_vers = Version::from_str(&lock_pack.version).unwrap();
                        if lock_pack.name == req.name {
                            let mut compatible = true;
                            for constraint in req.constraints.iter() {
                                if !constraint.is_compatible(&lock_vers) {
                                    compatible = false;
                                    break;
                                }
                            }
                            if compatible {
                                // Fix the constraint to the lock if compatible.
                                // todo printline temp
                                println!(
                                    "Locking constraint: {} → {}",
                                    &req.to_cfg_string(),
                                    lock_vers
                                );
                                req.constraints = vec![Constraint::new(
                                    dep_types::ReqType::Exact,
                                    lock_vers.major,
                                    lock_vers.minor,
                                    lock_vers.patch,
                                )];
                            }
                        }
                    }
                }
            }

            sync_deps(
                lock_filename,
                &bin_path,
                &lib_path,
                &merged_reqs,
                &installed,
                &py_vers,
            );
            util::print_color("Installation complete", Color::Green);
        }

        SubCommand::Uninstall { packages } => {
            let removed_reqs: Vec<String> = packages
                .into_iter()
                .map(|p| Req::from_str(&p, false).unwrap().name)
                .collect();

            edit_files::remove_reqs_from_cfg(cfg_filename, &removed_reqs);

            let updated_reqs: Vec<Req> = cfg
                .reqs
                .into_iter()
                .filter(|req| !removed_reqs.contains(&req.name))
                .collect();

            let installed = util::find_installed(&lib_path);
            sync_deps(
                lock_filename,
                &bin_path,
                &lib_path,
                &updated_reqs,
                &installed,
                &py_vers,
            );
            util::print_color("Uninstall complete", Color::Green);
        }

        SubCommand::Python { args } => {
            if commands::run_python(&bin_path, &lib_path, &args).is_err() {
                abort("Problem running Python");
            }
        }
        SubCommand::Package { extras } => build::build(&bin_path, &lib_path, &cfg, extras),
        SubCommand::Publish {} => build::publish(&bin_path, &cfg),
        SubCommand::Reset {} => {
            if fs::remove_dir_all(&pypackages_dir).is_err() {
                abort("Problem removing `__pypackages__` directory")
            }
            util::print_color("Reset complete", Color::Green);
        }

        SubCommand::Run { args } => {
            // Allow both `pypackage run ipython` (args), and `pypackage ipython` (opt.script)
            if !args.is_empty() {
                // todo better handling, eg abort
                let name = args.get(0).expect("Missing first arg").clone();
                let mut args: Vec<String> = args.into_iter().skip(1).collect();

                //                let scripts = vec![];
                let script_path = vers_path.join(format!("bin/{}", name));

                let abort_msg = &format!(
                    "Problem running the script {}. Is it installed? \
                     Try running `pypackage install {}`",
                    name, name
                );
                // Handle the error here, instead of letting Python handle it, so we can
                // display a more nicer message.
                if !script_path.exists() {
                    abort(abort_msg);
                }

                let mut args2 = vec![script_path.to_str().unwrap().to_owned()];
                //                for script in scripts {
                //                    args2.push(script);
                //                }
                args2.append(&mut args);

                if commands::run_python(&bin_path, &lib_path, &args2).is_err() {
                    abort(abort_msg);
                }

                return;
            }
        }
        SubCommand::List {} => util::show_installed(&lib_path),
        // We already handled init and new
        SubCommand::Init {} => (),
        SubCommand::New { name: _ } => (),
    }
}

#[cfg(test)]
pub mod tests {}
