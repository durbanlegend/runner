//! A Rust snippet runner.
//!
//! Please see [readme](https://github.com/stevedonovan/runner/blob/master/readme.md)
extern crate easy_shortcuts as es;
extern crate lapp;
#[macro_use] extern crate lazy_static;
extern crate semver;


use es::traits::*;
use std::process;
use std::env;
use std::fs;
use std::path::{Path,PathBuf};
use std::collections::HashMap;
use std::io::Write;

mod crate_utils;
mod platform;
mod strutil;
mod meta;

use std::env::consts::{EXE_SUFFIX,DLL_SUFFIX,DLL_PREFIX};

use platform::{open,edit};

use crate_utils::{RUSTUP_LIB, UNSTABLE};

const VERSION: &str = "0.3.1";

const USAGE: &str = "
Compile and run small Rust snippets
  -s, --static build statically (default is dynamic)
  -O, --optimize optimized static build
  -e, --expression evaluate an expression
  -i, --iterator iterate over an expression
  -n, --lines evaluate expression over stdin; the var 'line' is defined
  -x, --extern... (string) add an extern crate to the snippet
  -X, --wild... (string) like -x but implies wildcard import
  -p, --prepend (default '') put this statement in body (useful for -i etc)
  -N, --no-prelude do not include runner prelude
  -c, --compile-only  will not run program and copies it into current dir
  -r, --run  don't compile, only re-run

  Cache Management:
  --create (string...) initialize the static cache with crates
  --add  (string...) add new crates to the cache (after --create)
  --update update all, or a specific package given as argument
  --edit  edit the static cache Cargo.toml
  --build rebuild the static cache
  --cleanup clean out stale rlibs from cache
  --crates current crates and their versions in cache
  --doc  display documentation (any argument will be specific crate name)
  --edit-prelude edit the default prelude for snippets
  --alias (string...) crate aliases in form alias=crate_name (used with -x)

  Dynamic compilation:
  -P, --crate-path show path of crate source in Cargo cache
  -C, --compile  compile crate dynamically (limited)
  -L, --link (string) path for extra libraries
  --cfg... (string) pass configuration variables to rustc
  --features (string...) enable features in compilation
  --libc  link dynamically against libc (special case)
  (--extern is used to explicitly link in a crate by name)

  -v, --verbose describe what's happening
  -V, --version version of runner

  <program> (string) Rust program, snippet or expression
  <args> (string...) arguments to pass to program
";

// this will be initially written to ~/.cargo/.runner/prelude and
// can then be edited.
const PRELUDE: &str = "
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(unused_macros)]
use std::fs;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::env;
use std::path::{PathBuf,Path};
use std::collections::HashMap;

macro_rules! debug {
    ($x:expr) => {
        println!(\"{} = {:?}\",stringify!($x),$x);
    }
}
";

// a fairly arbitrary set of crates to start the ball rolling
// cf. https://github.com/brson/stdx
const KITCHEN_SINK: &str = "
    time
    regex
    toml
    serde_json json
    walkdir
    simple-error error-chain
    nom
    rayon pipeliner
    reqwest
    typed-arena
";

fn kitchen_sink(crates: Vec<String>) -> Vec<String> {
    if crates.len() == 1 && crates[0] == "kitchen-sink" {
        KITCHEN_SINK.split_whitespace().map(|s| s.into()).collect()
    } else {
        crates
    }
}

struct State {
    build_static: bool,
    optimize: bool,
    exe: bool,
}

impl State {
    fn exe(is_static: bool, optimized: bool) -> State {
        State {
            build_static: is_static,
            optimize: optimized,
            exe: true
        }
    }

    fn dll(optimized: bool) -> State {
        State {
            build_static: false,
            optimize: optimized,
            exe: false
        }
    }

}

fn main() {
    let args = lapp::parse_args(USAGE);
    let prelude = get_prelude();
    let b = |p| args.get_bool(p);

    if b("version") {
        println!("runner {}",VERSION);
        return;
    }
    let verbose = b("verbose");

    if b("run") && b("compile-only") {
        args.quit("--run and compile-only make no sense together");
    }


    let aliases = args.get_strings("alias");
    if aliases.len() > 0 {
        add_aliases(aliases);
        return;
    }

    if b("edit-prelude") {
        let rdir = runner_directory().join("prelude");
        edit(&rdir);
        return;
    }

    // Static Cache Management
    let crates = args.get_strings("create");
    if crates.len() > 0 {
        create_static_cache(&kitchen_sink(crates),true);
        return;
    }
    let crates = args.get_strings("add");
    if crates.len() > 0 {
        create_static_cache(&kitchen_sink(crates),false);
        return;
    }

    // operations on the static cache
    let (edit_toml, build, doc, update, cleanup, crates) =
        (b("edit"), b("build"), b("doc"), b("update"), b("cleanup"), b("crates"));

    if edit_toml || build || doc || update || cleanup || crates {
        let maybe_argument = args.get_string_result("program");
        let static_cache = static_cache_dir_check();
        if build || update {
            env::set_current_dir(&static_cache).or_die("static cache wasn't a directory?");
            if build {
                build_static_cache();
            } else {
                if let Ok(package) = maybe_argument {
                    cargo(&["update","--package",&package]);
                } else {
                    cargo(&["update"]);
                }
                return;
            }
        } else
        if doc {
            let the_crate = crate_utils::proper_crate_name(
                &if let Ok(file) =  maybe_argument {
                    file
                } else {
                    "static_cache".to_string()
                }
            );
            let docs = static_cache.join(&format!("target/doc/{}/index.html",the_crate));
            open(&docs);
        } else
        if cleanup {
            args.quit("cleanup not implemented yet");
        } else
        if crates {
            let mut m = meta::Meta::new_from_file(&static_cache);
            m.dump_crates(maybe_argument.ok());
        } else { // must be edit_toml
            let toml = static_cache.join("Cargo.toml");
            edit(&toml);
        }
        return;
    }

    let first_arg = args.get_string("program");
    let file = PathBuf::from(&first_arg);
    let optimized = args.get_bool("optimize");

    // Dynamically linking crates (experimental!)
    if b("crate-path") || b("compile") {
        let (crate_path,crate_name) = match crate_utils::crate_path(&file,&first_arg) {
            Ok(t) => t,
            Err(s) => args.quit(&s)
        };
        if b("crate-path") {
            println!("{}",crate_utils::cache_path(&crate_name).display());
        } else {
            println!("building crate '{}' at {}",crate_name, crate_path.display());
            let state = State::dll(optimized);
            compile_crate(&args, &state, &crate_name, &crate_path, None,  Vec::new());
        }
        return;
    }

    let state = State::exe(b("static"),optimized);

    // we'll pass rest of arguments to program
    let program_args = args.get_strings("args");

    let mut expression = true;
    let mut code = if b("expression") {
        // Evaluating an expression: just debug print it out.
        format!("println!(\"{{:?}}\",{});", quote(first_arg))
    } else
    if b("iterator") {
        // The expression is anything that implements IntoIterator
        format!("for val in {} {{\n println!(\"{{:?}}\",val);\n}}", quote(first_arg))
    } else
    if b("lines") {
        // The variable 'line' is available to an expression, evaluated for each line in stdin
        // But if the expression ends with '}' then don't dump out this value!
        let first_arg = quote(first_arg);
        let stmt = first_arg.trim_right().ends_with('}');
        let mut s = String::from("
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                let line = line?;
        ");
        s += &if ! stmt {
            format!("let val = {};\nprintln!(\"{{:?}}\",val);", first_arg)
        } else {
            format!("  {};",first_arg)
        };
        s += "\n}";
        s
    } else { // otherwise, just a file
        expression = false;
        es::read_to_string(&file)
    };

    // expressions may contain environment references like $PATH
    if expression {
        code = strutil::substitute(&code,"$",
            |c| c.is_alphanumeric() || c == '_',
            |s| {
                let text = if let Ok(num) = s.parse::<usize>() {
                    program_args.get(num-1).or_then_die(|_| format!("arg {} not found",num)).clone()
                } else {
                    env::var(s).or_then_die(|_| format!("$VAR {} not found",s))
                };
                format!("{:?}",text)
            }
        );
    }

    // ALL executables go into the Runner bin directory...
    let mut bin = runner_directory().join("bin");
    let mut externs = Vec::new();

    // proper Rust programs are accepted (this is a bit rough)
    let proper = code.find("fn main").is_some();
    let (rust_file, program) = if ! proper {
        // otherwise we must create a proper program from the snippet
        // and write this as a file in the Runner bin directory...
        let mut extern_crates = args.get_strings("extern");
        let wild_crates = args.get_strings("wild");
        if wild_crates.len() > 0 {
            extern_crates.extend(wild_crates.iter().cloned());
        }
        let mut extra = args.get_string("prepend");
        if ! extra.is_empty() {
            extra.push(';');
        }
        let maybe_prelude = if b("no-prelude") {
            "".into()
        } else {
            prelude
        };
        let (massaged_code, deduced_externs)
            = massage_snippet(code, maybe_prelude, extern_crates, wild_crates, extra);
        code = massaged_code;
        externs = deduced_externs;
        if ! expression {
            bin.push(&file);
            bin.set_extension("rs");
        } else { // we make up a name...
            bin.push("tmp.rs");
        }
        es::write_all(&bin,&code);
        let program = bin.with_extension(EXE_SUFFIX);
        (bin, program)
    } else {
        // the 'proper' case - use the file name part
        bin.push(file.file_name().unwrap());
        let program = bin.with_extension(EXE_SUFFIX);
        (file, program)
    };

    if b("run") {
        if ! program.exists() {
            args.quit(&format!("program {:?} does not exist",program));
        }
    } else {
        if ! compile_crate(&args,&state,"",&rust_file,Some(&program), externs) {
            return;
        }
        if verbose {
            println!("compiled {:?} successfully",rust_file);
        }
    }

    if b("compile-only") {
        let file_name = rust_file.file_name().or_die("no file name?");
        let home = crate_utils::cargo_home().join("bin");
        let here = home.join(file_name).with_extension(EXE_SUFFIX);
        println!("Copying {:?} to {:?}",program,here);
        fs::copy(&program,&here).or_die("cannot copy program");
        return;
    }


    // Finally run the compiled program
    let cache = get_cache(&state);
    let mut builder = process::Command::new(&program);
    if ! state.build_static {
        // must make the dynamic cache visible to the program!
        if cfg!(windows) {
            // Windows resolves DLL references on the PATH
            let path = env::var("PATH").unwrap();
            let new_path = format!("{};{}",path,cache.display());
            builder.env("PATH",new_path);
        } else {
            // whereas POSIX requires LD_LIBRARY_PATH
            builder.env("LD_LIBRARY_PATH",format!("{}:{}",*RUSTUP_LIB,cache.display()));
        }
    }
    builder.args(&program_args)
        .status()
        .or_then_die(|e| format!("can't run program {:?}: {}",program,e));
}

// handle two useful cases:
// - compile a crate as a dynamic library, given a name and an output dir
// - compile a program, given a program
fn compile_crate(args: &lapp::Args, state: &State,
    crate_name: &str, crate_path: &Path,
    output_program: Option<&Path>, mut extern_crates: Vec<String>) -> bool
{
    let verbose = args.get_bool("verbose");
    let debug = ! state.optimize;

    // implicit linking works fine, until it doesn't
    extern_crates.extend(args.get_strings("extern"));
    extern_crates.sort();
    extern_crates.dedup();
    // libc is such a special case
    if args.get_bool("libc") {
        extern_crates.push("libc".into());
    }
    let mut cfg = args.get_strings("cfg");
    for f in args.get_strings("features") {
        cfg.push(format!("feature=\"{}\"",f));
    }
    let cache = get_cache(&state);
    let mut builder = process::Command::new("rustc");
    if ! state.build_static { // stripped-down dynamic link
        builder.args(&["-C","prefer-dynamic"]).args(&["-C","debuginfo=0"]);
        if let Ok(link) = args.get_string_result("link") {
            if verbose { println!("linking against {}",link); }
            builder.arg("-L").arg(&link);
        }
    } else { // static build
        builder.arg(if state.optimize {"-O"} else {"-g"});
        if state.optimize {
            // no point in carrying around all that baggage...
            builder.args(&["-C","debuginfo=0"]);
        }
    }
    // implicitly linking against crates in the dynamic or static cache
    builder.arg("-L").arg(&cache);
    if ! state.exe { // as a dynamic library
        builder.args(&["--crate-type","dylib"])
        .arg("--out-dir").arg(&cache)
        .arg("--crate-name").arg(&crate_utils::proper_crate_name(crate_name));
    } else {
        builder.arg("-o").arg(output_program.unwrap());
    }
    for c in cfg {
        builder.arg("--cfg").arg(&c);
    }

    // explicit --extern references require special treatment for
    // static builds, since the libnames include a hash.
    // So we look for the latest crate of this name

    let extern_crates: Vec<(String,String)> =
    if state.build_static && extern_crates.len() > 0 {
        let m = meta::Meta::new_from_file(&static_cache_dir());
        extern_crates.into_iter().map(|c|
            (m.get_full_crate_name(&c,debug).or_then_die(|_| format!("no such crate '{}",c)),c)
        ).collect()
    } else {
        extern_crates.into_iter().map(|c|
            (format!("{}{}{}",DLL_PREFIX,c,DLL_SUFFIX),c)
        ).collect()
    };

    for (name,c) in extern_crates {
        let full_path = PathBuf::from(&cache).join(&name);
        let ext = format!("{}={}",c,full_path.display());
        if verbose {
            println!("extern {}",ext);
        }
        builder.arg("--extern").arg(&ext);
    }
    builder.arg(crate_path);
    builder.status().or_die("can't run rustc").success()
}

// Windows shell quoting is a mess, so we make single quotes
// become double quotes in expressions
fn quote(s: String) -> String {
    if cfg!(windows) {
        s.replace("\'","\"")
    } else {
        s
    }
}

fn runner_directory() -> PathBuf {
    let mut runner = crate_utils::cargo_home().join(".runner");
    if *UNSTABLE {
        runner.push("unstable");
    }
    runner
}

fn cargo(args: &[&str]) -> bool {
    let res = process::Command::new("cargo")
        .args(args)
        .status()
        .or_die("can't run cargo");
    res.success()
}

fn cargo_build(release: bool) -> Option<String> {
    use process::Stdio;
    use std::io::BufReader;
    use std::io::prelude::*;

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
    for line in inb.lines() {
        if let Ok(line) = line {
            if line.starts_with('{') {
                out += &line;
                out.push('\n');
            } else {
                println!("{}",line);
            }
        }
    }

    if res.wait().or_die("cargo build error").success() {
        Some(out)
    } else {
        None
    }
}

const STATIC_CACHE: &str = "static-cache";
const DYNAMIC_CACHE: &str = "dy-cache";

fn static_cache_dir() -> PathBuf {
    runner_directory().join(STATIC_CACHE)
}

fn static_cache_dir_check() -> PathBuf {
    let static_cache = static_cache_dir();
    if ! static_cache.exists() {
        es::quit("please build static cache with --create first");
    }
    static_cache
}

fn build_static_cache() -> bool {
    use meta::*;
    let mut m = Meta::new();
    match cargo_build(false) {
        None => return false,
        Some(s) => m.debug(s)
    }
    match cargo_build(true) {
        None => return false,
        Some(s) => m.release(s)
    }
    m.update(&static_cache_dir());
    cargo(&["doc"])
}


fn create_static_cache(crates: &[String], please_create: bool) {
    use std::io::prelude::*;

    // this is called with `true` for "--create" and `false` for "--add".
    let static_cache = static_cache_dir();
    let exists = static_cache.exists();

    let mut create = please_create;
    // It is fine to start with "add" since action is obvious...
    if ! create && ! exists {
        create = true;
    } else
    // but not to do "create" after static cache has been created
    if create && exists {
        es::quit("static cache already created - use --add");
    }

    let mut home = runner_directory();
    env::set_current_dir(&home).or_die("cannot change to home directory");
    if create {
        if ! cargo(&["new","--bin",STATIC_CACHE]) {
            es::quit("cannot create static cache");
        }
    }
    home.push(STATIC_CACHE);
    env::set_current_dir(&home).or_die("could not change to static cache directory");
    let tmpfile = env::temp_dir().join("Cargo.toml");
    fs::copy("Cargo.toml",&tmpfile).or_die("cannot back up Cargo.toml");
    {
        let mut deps = fs::OpenOptions::new().append(true)
            .open("Cargo.toml").or_die("could not append to Cargo.toml");
        for c in crates {
            if let None = c.find('=') {
                write!(deps,"{}=\"*\"\n",c)
            } else {
                write!(deps,"{}\n",c)
            }.or_die("could not modify Cargo.toml");
        }
    }
    if ! build_static_cache() {
        println!("Error occurred - restoring Cargo.toml");
        fs::copy(&tmpfile,"Cargo.toml").or_die("cannot restore Cargo.toml");
    }
}

// this is always called first and has the important role to ensure that
// runner's directory structure is created properly.
fn get_prelude() -> String {
    let home = runner_directory();
    let pristine = ! home.is_dir();
    if pristine {
        fs::create_dir_all(&home).or_die("cannot create runner directory");
    }
    let prelude = home.join("prelude");
    let bin = home.join("bin");
    if pristine {
        es::write_all(&prelude,PRELUDE);
        fs::create_dir(&home.join(DYNAMIC_CACHE)).or_die("cannot create dynamic cache");
    }
    if pristine || ! bin.is_dir() {
        fs::create_dir(&bin).or_die("cannot create output directory");
    }
    es::read_to_string(&prelude)
}

fn get_cache(state: &State) -> PathBuf {
    let mut home = runner_directory();
    if state.build_static {
        home.push(STATIC_CACHE);
        home.push("target");
        home.push(if state.optimize {"release"} else {"debug"});
        home.push("deps");
    } else {
        home.push(DYNAMIC_CACHE);
    };
    home
}

fn add_aliases(aliases: Vec<String>) {
    if aliases.len() == 0 { return; }
    let alias_file = runner_directory().join("alias");
    let mut f = if alias_file.is_file() {
        fs::OpenOptions::new().append(true).open(&alias_file)
    } else {
        fs::File::create(&alias_file)
    }.or_die("cannot open runner alias file");

    for crate_alias in aliases {
        write!(f,"{}\n",crate_alias).or_die("cannot write to runner alias file");
    }
}

fn get_aliases() -> HashMap<String,String> {
    let alias_file = runner_directory().join("alias");
    if ! alias_file.is_file() { return HashMap::new(); }
    es::lines(es::open(&alias_file))
      .filter_map(|s| s.split_at_delim('=').trim()) // split into (String,String)
      .to_map()
}

fn massage_snippet(code: String, prelude: String,
        extern_crates: Vec<String>, wild_crates: Vec<String>, body_prelude: String) -> (String,Vec<String>) {
    use strutil::{after,word_after};

    fn indent_line(line: &str) -> String {
        format!("    {}\n",line)
    }

    let mut prefix = prelude;
    let mut crate_begin = String::new();
    let mut body = String::new();
    let mut deduced_externs = Vec::new();

    body += &body_prelude;
    {
        if extern_crates.len() > 0 {
            let aliases = get_aliases();
            for c in &extern_crates {
                prefix += &if let Some(aliased) = aliases.get(c) {
                    format!("extern crate {} as {};\n",aliased,c)
                } else {
                    format!("extern crate {};\n",c)
                };
            }
            for c in wild_crates {
                prefix += &format!("use {}::*;\n",c);
            }
        }
        let mut lines = code.lines();
        let mut first = true;
        for line in lines.by_ref() {
            let line = line.trim_left();
            if first { // files may start with #! shebang...
                if line.starts_with("#!/") {
                    continue;
                }
                first = false;
            }
            // crate import, use should go at the top.
            // Particularly need to force crate-level attributes to the top
            // These must not be in the `run` function we're generating
            if let Some(rest) = after(line,"#[macro_use") {
                if let Some(crate_name) = word_after(rest,"extern crate ") {
                    deduced_externs.push(crate_name);
                }
                prefix += line;
                prefix.push('\n');
            } else
            if line.starts_with("extern ") || line.starts_with("use ") {
                if let Some(crate_name) = word_after(line,"extern crate ") {
                    deduced_externs.push(crate_name);
                }
                prefix += line;
                prefix.push('\n');
            } else
            if line.starts_with("#![") {
                // inner attributes really need to be at the top of the file
                crate_begin += line;
                crate_begin.push('\n');
            } else
            if line.len() > 0 {
                body += &indent_line(line);
                break;
            }
        }
        // and indent the rest!
        body.extend(lines.map(indent_line));
    }

    deduced_externs.extend(extern_crates);
    deduced_externs.sort();
    deduced_externs.dedup();

    let massaged_code = format!("{}
{}

fn run() -> std::result::Result<(),Box<std::error::Error>> {{
{}    Ok(())
}}
fn main() {{
    if let Err(e) = run() {{
        println!(\"error: {{:?}}\",e);
    }}
}}
",crate_begin,prefix,body);

    (massaged_code, deduced_externs)

}

