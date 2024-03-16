// cache management

use crate::es::traits::{Die, MaybeTrim, StringEx, ToMap, ToVec};
use std::collections::HashMap;
use std::convert::Into;
use std::env;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process;

use crate::crate_utils;
use crate::meta;

use crate::compile;
use crate::platform::{edit, open};
use crate_utils::UNSTABLE;
use lapp::Args;
use std::ops::ControlFlow;

use crate::state::State;

const STATIC_CACHE: &str = "static-cache";
const DYNAMIC_CACHE: &str = "dy-cache";

// this will be initially written to ~/.cargo/.runner/prelude and
// can then be edited.
const PRELUDE: &str = "
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_macros)]
use std::{fs,io,env};
use std::fs::File;
use std::io::prelude::*;
use std::path::{PathBuf,Path};
use std::collections::HashMap;
use std::time::Duration;
use std::thread;

macro_rules! debug {
    ($x:expr) => {
        println!(\"{} = {:?}\",stringify!($x),$x);
    }
}
";

// a fairly arbitrary set of crates to start the ball rolling
// cf. https://github.com/brson/stdx
const KITCHEN_SINK: &str = "
    chrono
    regex
    serde_json
    serde_yaml
";

// Windows shell quoting is a mess, so we make single quotes
// become double quotes in expressions
pub fn quote(s: String) -> String {
    if cfg!(all(windows, not(feature = "no_quote_replacement"))) {
        s.replace('\'', "\"")
    } else {
        s
    }
}

pub fn runner_directory() -> PathBuf {
    let mut runner = crate_utils::cargo_home().join(".runner");
    if *UNSTABLE {
        runner.push("unstable");
    }
    runner
}

pub fn cargo(args: &[&str]) -> bool {
    let res = process::Command::new("cargo")
        .args(args)
        .status()
        .or_die("can't run cargo");
    res.success()
}

pub fn cargo_build(release: bool) -> Option<String> {
    use process::Stdio;
    use std::io::BufReader;

    let mut c = process::Command::new("cargo");
    c.arg("build");
    if release {
        c.arg("--release");
    }
    c.stdout(Stdio::piped());
    c.arg("--message-format").arg("json");

    let mut res = c.spawn().or_die("can't run cargo");

    // collect all JSON records, and let the rest
    // pass through...
    let inb = BufReader::new(res.stdout.take().unwrap());
    let mut out = String::new();
    inb.lines().map_while(Result::ok).for_each(|line| {
        if line.starts_with('{') {
            out += &line;
            out.push('\n');
        } else {
            println!("{line}");
        }
    });

    if res.wait().or_die("cargo build error").success() {
        Some(out)
    } else {
        None
    }
}

pub fn static_cache_dir() -> PathBuf {
    runner_directory().join(STATIC_CACHE)
}

pub fn get_metadata() -> meta::Meta {
    let static_cache = static_cache_dir();
    if meta::Meta::exists(&static_cache) {
        meta::Meta::new_from_file(&static_cache)
    } else {
        es::quit("please build the static cache with `runner --add <crate>...` first");
    }
}

pub fn static_cache_dir_check() -> PathBuf {
    let static_cache = static_cache_dir();
    if !static_cache.exists() {
        es::quit("please build the static cache with `runner --add <crate>...` first");
    }
    static_cache
}

pub fn build_static() -> bool {
    use crate::meta::Meta;
    let mut m = Meta::new();
    match cargo_build(false) {
        None => return false,
        Some(s) => m.debug(&s),
    }
    match cargo_build(true) {
        None => return false,
        Some(s) => m.release(&s),
    }
    m.update(&static_cache_dir());
    cargo(&["doc"])
}

pub fn create_static(crates: &[String]) {
    let static_cache = static_cache_dir();
    let exists = static_cache.exists();

    let crates = if crates.len() == 1 && crates[0] == "kitchen-sink" {
        KITCHEN_SINK.split_whitespace().map(Into::into).collect()
    } else {
        crates.to_vec()
    };

    let mut home = runner_directory();
    env::set_current_dir(&home).or_die("cannot change to home directory");

    let mdata = if exists {
        Some(get_metadata())
    } else {
        if !cargo(&["new", "--bin", STATIC_CACHE]) {
            es::quit("cannot create static cache");
        }
        None
    };
    let check_crate = |s: &str| {
        if let Some(m) = &mdata {
            m.is_crate_present(s)
        } else {
            false
        }
    };

    // there are three forms possible
    // a plain crate name - we assume latest version ('*')
    // a name=vs - we'll ensure it gets quoted properly
    // a local Cargo project
    let crates_vs = crates
        .iter()
        .filter_map(|c| {
            if let Some(idx) = c.find('=') {
                // help with a little bit of quoting...
                let (name, vs) = (&c[0..idx], &c[(idx + 1)..]);
                Some((name.to_string(), vs.to_string(), true))
            } else {
                // explicit name but no version, see if we already have this crate
                if let Some((name, path)) = maybe_cargo_dir(c) {
                    // hello - this is a local Cargo project!
                    if check_crate(&name) {
                        None
                    } else {
                        Some((name, path.to_str().unwrap().to_string(), false))
                    }
                } else {
                    // latest version of crate
                    if check_crate(c) {
                        None
                    } else {
                        Some((c.to_string(), '*'.to_string(), true))
                    }
                }
            }
        })
        .to_vec();

    if crates_vs.is_empty() {
        return;
    }

    home.push(STATIC_CACHE);
    env::set_current_dir(&home).or_die("could not change to static cache directory");
    let tmpfile = env::temp_dir().join("Cargo.toml");
    fs::copy("Cargo.toml", &tmpfile).or_die("cannot back up Cargo.toml");
    {
        let mut deps = fs::OpenOptions::new()
            .append(true)
            .open("Cargo.toml")
            .or_die("could not append to Cargo.toml");
        for (name, vs, semver) in crates_vs {
            if semver {
                writeln!(deps, "{name}=\"{vs}\"")
            } else {
                writeln!(deps, "{name}={{path=\"{vs}\"}}")
            }
            .or_die("could not modify Cargo.toml");
        }
    }
    if !build_static() {
        println!("Error occurred - restoring Cargo.toml");
        fs::copy(&tmpfile, "Cargo.toml").or_die("cannot restore Cargo.toml");
    }
}

fn maybe_cargo_dir(name: &str) -> Option<(String, PathBuf)> {
    let path = Path::new(name);
    if !path.exists() || !path.is_dir() {
        return None;
    }
    let full_path = path.canonicalize().or_die("bad path, man!");
    if let Ok((full_path, cargo_toml)) = crate_utils::cargo_dir(&full_path) {
        let name = crate_utils::crate_info(&cargo_toml).name;
        Some((name, full_path))
    } else {
        None
    }
}

// this is always called first and has the important role to ensure that
// runner's directory structure is created properly.
pub fn get_prelude() -> String {
    let home = runner_directory();
    let pristine = !home.is_dir();
    if pristine {
        fs::create_dir_all(&home).or_die("cannot create runner directory");
    }
    let prelude = home.join("prelude");
    let bin = home.join("bin");
    if pristine {
        fs::write(&prelude, PRELUDE).or_die("cannot write prelude");
        fs::create_dir(home.join(DYNAMIC_CACHE)).or_die("cannot create dynamic cache");
    }
    if pristine || !bin.is_dir() {
        fs::create_dir(&bin).or_die("cannot create output directory");
    }
    fs::read_to_string(&prelude).or_die("cannot read prelude")
}

#[allow(clippy::module_name_repetitions)]
pub fn get_cache(state: &State) -> PathBuf {
    let mut home = runner_directory();
    if state.build_static {
        home.push(STATIC_CACHE);
        home.push("target");
        home.push(if state.optimize { "release" } else { "debug" });
        home.push("deps");
    } else {
        home.push(DYNAMIC_CACHE);
    };
    home
}

pub fn add_aliases(aliases: Vec<String>) {
    if aliases.is_empty() {
        return;
    }
    let alias_file = runner_directory().join("alias");
    let mut f = if alias_file.is_file() {
        fs::OpenOptions::new().append(true).open(&alias_file)
    } else {
        fs::File::create(&alias_file)
    }
    .or_die("cannot open runner alias file");

    for crate_alias in aliases {
        writeln!(f, "{crate_alias}").or_die("cannot write to runner alias file");
    }
}

pub fn get_aliases() -> HashMap<String, String> {
    let alias_file = runner_directory().join("alias");
    if !alias_file.is_file() {
        return HashMap::new();
    }
    let contents = fs::read_to_string(&alias_file).or_die("cannot read alias file");
    contents
        .lines()
        .filter_map(|s| s.split_at_delim('=').trim()) // split into (String,String)
        .to_map()
}

pub fn static_cache_ops(args: &Args<'_>, rs_file_contents: &Option<String>) -> ControlFlow<()> {
    let b = |p: &str| args.get_bool(p);

    let verbose = b("verbose");

    let crates = args.get_strings("add");
    if !crates.is_empty() {
        create_static(&crates);
        if rs_file_contents.is_none() {
            return ControlFlow::Break(());
        }
    }
    let (edit_toml, build, doc, update, cleanup, crates) = (
        b("edit"),
        b("build"),
        b("doc"),
        b("update"),
        b("cleanup"),
        b("crates"),
    );

    // operations on the static cache

    if edit_toml || build || doc || update || cleanup || crates {
        let maybe_argument = args.get_string_result("program");
        let static_cache = static_cache_dir_check();
        if build || update {
            env::set_current_dir(&static_cache).or_die("static cache wasn't a directory?");
            if build {
                build_static();
            } else {
                if let Ok(package) = maybe_argument {
                    cargo(&["update", "--package", &package]);
                } else {
                    cargo(&["update"]);
                }
                return ControlFlow::Break(());
            }
        } else if doc {
            let the_crate = crate_utils::proper_crate_name(&if let Ok(file) = maybe_argument {
                file
            } else {
                "static_cache".to_string()
            });
            let docs = static_cache.join(format!("target/doc/{the_crate}/index.html"));
            open(&docs);
        } else if cleanup {
            cargo(&["clean"]);
        } else if crates {
            let mut m = get_metadata();
            let mut crates = Vec::new();
            if let Ok(name) = maybe_argument {
                crates.push(name);
                crates.extend(args.get_strings("args"));
            }
            m.dump_crates(crates, verbose);
        } else {
            // must be edit_toml
            let toml = static_cache.join("Cargo.toml");
            edit(&toml);
        }
        return ControlFlow::Break(());
    }
    ControlFlow::Continue(())
}

pub fn dynamic_crate_ops(
    optimized: bool,
    edition: &str,
    crate_name: &str,
    args: &Args<'_>,
    print_path: bool,
    compile: bool,
    maybe_src_path: &Option<PathBuf>,
) -> ControlFlow<()> {
    let mut state = State::dll(optimized, edition);
    // plain-jane name is a crate name!
    if crate_utils::plain_name(crate_name) {
        // but is it one of Ours? Then we definitely know what the
        // actual crate name is AND where the source is cached
        let m = get_metadata();
        if let Some(e) = m.get_meta_entry(crate_name) {
            if e.path == Path::new("") {
                args.quit("please run 'runner --build' to update metadata");
            }
            // will be <cargo dir>/src/FILE.rs
            let path = e.path.parent().unwrap().parent().unwrap();
            if print_path {
                println!("{}", path.display());
            } else {
                let ci = crate_utils::crate_info(&path.join("Cargo.toml"));
                // respect the crate's edition!
                state.edition = ci.edition;
                // TBD can override --features with features actually
                // used to build this crate
                let build_features = &e.features;
                eprintln!(
                    "dynamically linking crate '{}' with features [{}] at {}",
                    e.crate_name,
                    build_features,
                    e.path.display()
                );
                compile::dlib_or_prog(
                    args,
                    &state,
                    &e.crate_name,
                    &e.path,
                    None,
                    Vec::new(),
                    build_features
                        .split_whitespace()
                        .map(ToString::to_string)
                        .collect(),
                );
            }
            return ControlFlow::Break(());
        }
    } else {
        if compile {
            if let Some(file) = maybe_src_path {
                if !file.exists() {
                    args.quit("no such file or directory for crate compile");
                }
                let (crate_name, crate_path) = if file.is_dir() {
                    match crate_utils::cargo_dir(file) {
                        Ok((path, cargo_toml)) => {
                            // this is somewhat dodgy, since the default location can be changed
                            // Safest bet is to add the crate to the runner static cache
                            let source = path.join("src").join("lib.rs");
                            let ci = crate_utils::crate_info(&cargo_toml);
                            // respect the crate's edition!
                            state.edition = ci.edition;
                            (ci.name, source)
                        }
                        Err(msg) => args.quit(&msg),
                    }
                } else {
                    // should be just a Rust source file
                    if file.extension().or_die("expecting extension") != "rs" {
                        args.quit(
                            "expecting known crate, dir containing Cargo.toml or Rust source file",
                        );
                    }
                    let name = crate_utils::path_file_name(&file.with_extension(""));
                    (name, file.clone())
                };
                eprintln!(
                    "compiling crate '{}' at {}",
                    crate_name,
                    crate_path.display()
                );
                compile::dlib_or_prog(
                    args,
                    &state,
                    &crate_name,
                    &crate_path,
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                return ControlFlow::Break(());
            }
            args.quit("--compile specified with no crate name");
        }
        // we no longer go for wild goose chase to find crates in the Cargo cache
        args.quit("not found in the static cache");
    }
    ControlFlow::Continue(())
}
