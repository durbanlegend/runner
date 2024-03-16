//! A Rust snippet runner.
//!
//! Please see [readme](https://github.com/stevedonovan/runner/blob/master/readme.md)
extern crate easy_shortcuts as es;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;

use es::traits::Die;
use lapp::Args;

use std::env::consts::EXE_SUFFIX;
use std::fs;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process;

use std::string::ToString;
use std::{env, io};

mod cache;
mod cargo_lock;
mod compile;
mod crate_utils;
mod meta;
mod platform;
mod snippet;
mod state;
mod strutil;

use crate::crate_utils::RUSTUP_LIB;
use cache::quote;
use compile::check_well_formed;
use platform::edit;
use state::State;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "
Compile and run small Rust snippets
  -s, --static build statically (default is dynamic)
  -d, --dynamic overrides --static in env.rs
  -O, --optimize optimized static build
  -e, --expression evaluate an expression - try enclosing in braces if having trouble
  -i, --iterator iterate over an expression
  -n, --lines evaluate expression over stdin; the var 'line' is defined
  -x, --extern... (string) add an extern crate to the snippet
  -X, --wild... (string) like -x but implies wildcard import
  -M, --macro... (string) like -x but implies macro import
  -p, --prepend (default '') put this statement in body (useful for -i etc)
  -N, --no-prelude do not include runner prelude
  -c, --compile-only  compiles program and copies to output dir
  -o, --output (path default cargo) change the default output dir for compilation
  -r, --run  don't compile, only re-run
  -S, --no-simplify by default, attempt to simplify rustc error messages
  -E, --edition (default '2021') specify Rust edition
  -I, --stdin Input from stdin

  Cache Management:
  --add  (string...) add new crates to the static cache
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

fn read_rs_file_contents(file: &Path) -> String {
    fs::read_to_string(file).or_die("cannot read file")
}

/// Read first line of source file, skipping any shebang, and interpret any arguments prefixed by "//: ".
fn update_args_from_src(rs_file_name: &str, contents: &str, args: &mut Args<'_>) {
    let lines = &mut contents.lines();
    let mut first_line = lines.next().or_die("empty file");
    if first_line.starts_with("#!") {
        first_line = lines.next().or_die("premature end of file after shebang");
    }
    let arg_comment = "//: ";
    let has_arg_comment = first_line.starts_with(arg_comment);
    if has_arg_comment {
        let default_arg_str = &first_line[arg_comment.len()..];
        let default_args = shlex::split(default_arg_str).or_die("bad comment args");

        args.parse_command_line(default_args.clone())
            .or_die("cannot parse comment args");
        args.clear_used();
        args.parse_env_args().or_die("bad command line");
        if args.get_bool("verbose") {
            eprintln!("Picked up arguments from {rs_file_name}: {default_args:?}");
        }
    }
}

pub struct PrettyError<T>(pub T);

impl<T> From<T> for PrettyError<T> {
    fn from(v: T) -> Self {
        Self(v)
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
// TODO: remove #[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_lines)]
fn main() {
    let start = std::time::Instant::now();

    // Retrieve and process command-line arguments, and any stored in snippet files or ./env.rs.
    let mut args = get_args();

    // Get contents of .rs file if provided
    let rs_file_contents = get_rs_file_contents(&mut args);
    eprintln!("rs_file_contents={rs_file_contents:?}");

    let prelude = get_prelude(&mut args);
    eprintln!("prelude=[{prelude:?}]");

    let exe_suffix = if EXE_SUFFIX.is_empty() {
        ""
    } else {
        &EXE_SUFFIX[1..]
    };

    // let b = |p: &str| bool_var(p, &args);
    if bool_var("version", &args) {
        println!("runner {VERSION}");
        return;
    }
    let verbose = bool_var("verbose", &args);

    // Quit with message if meaningless option combinations specified
    check_combos(&args);

    let aliases = args.get_strings("alias");
    if !aliases.is_empty() {
        cache::add_aliases(aliases);
        return;
    }

    if bool_var("edit-prelude", &args) {
        let rdir = cache::runner_directory().join("prelude");
        edit(&rdir);
        return;
    }

    // Static Cache Management
    // TODO: see if we can avoid this method for program or dynamic crate ops
    if let ControlFlow::Break(()) = cache::static_cache_ops(&args, &rs_file_contents) {
        return;
    }

    let static_state = bool_var("static", &args) && !bool_var("dynamic", &args);

    // Decide how to process request
    eprintln!("Before maybe_src_name");
    let maybe_src_name = assign_source_name(&args);
    eprintln!("After maybe_src_name");
    let maybe_src_path: Option<PathBuf> = maybe_src_name.as_ref().map(PathBuf::from);
    eprintln!("maybe_src_path={maybe_src_path:?}");

    let optimized = args.get_bool("optimize");
    let edition = args.get_string("edition");

    // Dynamically linking crates (experimental!)
    let (print_path, compile) = (bool_var("crate-path", &args), bool_var("compile", &args));
    if print_path || compile {
        let Some(crate_name) = &maybe_src_name else {
            args.quit("crate operation requested with no crate name")
        };
        if let ControlFlow::Break(()) = cache::dynamic_crate_ops(
            optimized,
            &edition,
            crate_name,
            &args,
            print_path,
            compile,
            &maybe_src_path,
        ) {
            return;
        }
    }

    let state = State::exe(static_state, optimized, &edition);

    // Prepare Rust code.
    let (args, program_args, source_file, has_save_name, raw_code) =
        prepare_rust_code(args, maybe_src_name, rs_file_contents);

    // Check if already a program
    let well_formed = if bool_var("iterator", &args) || bool_var("lines", &args) {
        false
    } else {
        // eprintln!("Checking if snippet has a fn main, and if so, does it compile?...");
        check_well_formed(verbose, &raw_code)
    };

    // Special handling for different cases. Convert code according to type.
    eprintln!("Before preprocess_code_type");
    let code = preprocess_code_type(&args, well_formed, raw_code);
    eprintln!("After preprocess_code_type");

    // ALL source and executables go into the Runner bin directory...
    let target_dir = cache::runner_directory().join("bin");
    let mut src_path: PathBuf = target_dir.clone();
    let mut externs = Vec::new();

    // If code is a snippet, transform it into a Rust program.
    // 'Proper' (well-formed) Rust programs are accepted
    let (rs_name, exe_path) = if well_formed {
        eprintln!("Before finalize_program");
        finalize_program(
            &code,
            &mut externs,
            maybe_src_path.clone(),
            &target_dir,
            exe_suffix,
        )
    } else {
        // otherwise we must create a proper program from the snippet
        // and write this as a file in the Runner bin directory...
        eprintln!("Before snippet_to_program");
        let mut rs_path = target_dir.clone();
        {
            let code = snippet::snippet_to_program(&args, &code, &edition, &mut externs, prelude);
            if !source_file && !has_save_name {
                // we make up a name...
                rs_path.push("tmp.rs");
            } else {
                let rs_name = maybe_src_path.clone().or_die("no such file or directory");
                rs_path.push(rs_name.file_name().unwrap());
                rs_path.set_extension("rs");
            }
            eprintln!("1. Writing code for {maybe_src_path:?} to bin={target_dir:?}");
            fs::write(&rs_path, code).or_die("cannot write code");
        }
        let mut exe_path = target_dir.clone();
        let exe_stem = maybe_src_path.clone().or_die("no such file or directory");
        exe_path.push(exe_stem.file_name().unwrap());
        exe_path.set_extension(exe_suffix);
        (rs_path, exe_path)
    };

    eprintln!("rs_name={rs_name:?}");
    // Compile program unless running precompiled
    src_path.push(rs_name);
    let rs_path = src_path.with_extension("rs");

    eprintln!("Before compile::program");
    if let ControlFlow::Break(()) = compile::program(
        &exe_path, &args, verbose, &state, &rs_path, externs, exe_suffix,
    ) {
        eprintln!("After compile::program");
        return;
    }

    // Run Rust code
    // Ready program environment for execution
    eprintln!("Before get_ready");
    let builder = get_ready(&state, &exe_path, verbose, &args);
    eprintln!("After get_ready");

    // Finally run the compiled program
    eprintln!("Before run");
    run(verbose, builder, &program_args, &exe_path);
    eprintln!("After run");

    // if verbose {
    let dur = start.elapsed();
    eprintln!("Completed in {}.{}s", dur.as_secs(), dur.subsec_millis());
    // }
}

fn assign_source_name(args: &Args<'_>) -> Option<String> {
    let b = |p: &str| bool_var(p, args);
    if b("run") {
        let mode_req = if b("static") { "static" } else { "dynamic" };
        eprintln!("Flag --{mode_req} will be ignored since program is precompiled");
    }
    let maybe_src_name: Option<String> = if b("stdin") && !b("compile-only") {
        // eprintln!("1. program=stdin");
        Some("stdin".to_string())
    } else if b("expression") || b("iterator") || b("lines") {
        Some("tmp.rs".to_string())
    } else {
        eprintln!("2. assigning first_arg value as maybe_src_name");
        let first_arg = args.get_string("program");
        Some(first_arg)
    };
    maybe_src_name
}

// Retrieve the command-line arguments
fn get_args() -> Args<'static> {
    let args = {
        let mut args = Args::new(USAGE);
        args.parse_spec().or_die("bad spec");
        args.parse_env_args().or_die("bad command line");
        args
    };
    args
}

fn get_ready(state: &State, program: &PathBuf, verbose: bool, args: &Args<'_>) -> process::Command {
    let b = |p: &str| args.get_bool(p);

    let ch = cache::get_cache(state);
    let mut builder = process::Command::new(program);
    if state.build_static {
        if verbose && !b("run") {
            eprintln!("Running statically");
        }
    } else {
        if verbose && !b("run") {
            eprintln!("Running program ({program:?}) dynamically");
        }
        // must make the dynamic cache visible to the program!
        if cfg!(windows) {
            // Windows resolves DLL references on the PATH
            let path = env::var("PATH").unwrap();
            let new_path = format!("{};{}", path, ch.display());
            builder.env("PATH", new_path);
        } else {
            // whereas POSIX requires LD_LIBRARY_PATH
            builder.env(
                "LD_LIBRARY_PATH",
                format!("{}:{}", *RUSTUP_LIB, ch.display()),
            );
        }
        builder.env(
            "DYLD_FALLBACK_LIBRARY_PATH",
            format!("{}:{}", ch.display(), *RUSTUP_LIB),
        );
    }
    if verbose {
        eprintln!(
            "Running {program:?} with environment [{:?}] and args [{:?}]",
            builder.get_envs(),
            builder.get_args()
        );
    }
    builder
}

fn run(verbose: bool, mut builder: process::Command, program_args: &[String], program: &PathBuf) {
    if verbose {
        eprintln!("About to execute program {builder:?}");
    }
    let dash_line = "-".repeat(50);
    println!("{dash_line}");
    let status = builder
        .args(program_args)
        .status()
        .or_then_die(|e| format!("can't run program {program:?}: {e}"));
    if !status.success() {
        process::exit(status.code().unwrap_or(-1));
    }
    println!("{dash_line}");
}

fn finalize_program(
    code: &str,
    externs: &mut Vec<String>,
    maybe_src_path: Option<PathBuf>,
    target_dir: &Path,
    exe_suffix: &str,
) -> (PathBuf, PathBuf) {
    for line in code.lines() {
        if let Some(crate_name) = strutil::word_after(line, "extern crate ") {
            externs.push(crate_name);
        }
    }
    // the 'proper' case - use the file name part
    let file = maybe_src_path.or_die("no such file or directory as requested for source program");
    let mut path_stem = target_dir.to_path_buf();
    path_stem.push(file.file_name().unwrap());
    let exe_path = path_stem.with_extension(exe_suffix); // Path of final executable
    let rs_path = target_dir.with_extension("rs"); // Path of final source file

    // eprintln!("2. Writing code for {file:?} to rust_path={rs_path:?}");
    // eprintln!("In finalize_program: head of code is:");
    // for line in code.lines().take(10) {
    //     eprintln!("{line}");
    // }

    fs::write(&rs_path, code).or_die("cannot write code");
    (rs_path, exe_path)
}

fn preprocess_code_type(args: &Args<'_>, well_formed: bool, raw_code: String) -> String {
    let b = |p: &str| bool_var(p, args);
    let code = if b("expression") {
        if well_formed {
            raw_code
        } else {
            // Evaluating an expression: just debug print it out.
            let expr_code = format!("println!(\"{{:?}}\",{});", raw_code.trim_end());
            // eprintln!("\nexpr_code={expr_code}\n");
            expr_code
        }
    } else if b("iterator") {
        // The expression is anything that implements IntoIterator
        format!("for val in {raw_code} {{\n println!(\"{{:?}}\",val);\n}}")
    } else if b("lines") {
        // The variable 'line' is available to an expression, evaluated for each line in stdin
        // But if the expression ends with '}' then don't dump out this value!
        let mut s = String::from(
            "
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                let line = line?;
        ",
        );
        s += &if raw_code.trim_end().ends_with('}') {
            format!("  {raw_code};")
        } else {
            format!("let val = {raw_code};\nprintln!(\"{{:?}}\",val);")
        };
        s += "\n}";
        s
    } else {
        raw_code.trim_end().to_string()
    };
    eprintln!("In preprocess_code_type: head of code is:");
    for line in code.lines().take(10) {
        eprintln!("{line}");
    }
    code
}

fn prepare_rust_code(
    mut args: Args<'_>,
    first_arg: Option<String>,
    rs_file_contents: Option<String>,
) -> (Args<'_>, Vec<String>, bool, bool, String) {
    // let b = |p: &str| bool_var(p, &args);
    let program_args = args.get_strings("args");

    let mut source_file = false;
    let (has_save_name, raw_code) = if bool_var("stdin", &args) {
        let mut s = String::new();
        io::stdin()
            .lock()
            .read_to_string(&mut s)
            .or_die("could not read from stdin");

        // println!("Content from stdin:\n{}", s);
        update_args_from_src("stdin", &s, &mut args);
        (
            bool_var("compile-only", &args) || first_arg.is_some(),
            quote(s),
        )
    } else if bool_var("expression", &args)
        || bool_var("iterator", &args)
        || bool_var("lines", &args)
    {
        // let file = file_res.clone().or_die("no such file or directory");

        let first_arg = args.get_string("program");
        eprintln!("first_arg={first_arg}");

        (false, quote(first_arg))
    } else {
        // otherwise, just a file
        source_file = true;
        (true, rs_file_contents.or_die("no .rs file"))
    };
    (args, program_args, source_file, has_save_name, raw_code)
}

fn check_combos(args: &Args<'_>) {
    let b = |p: &str| bool_var(p, args);
    if b("run") && b("compile-only") {
        args.quit("--run and compile-only make no sense together");
    }
    if b("lines") && b("stdin") {
        args.quit("--lines and stdin make no sense together, as lines already reads from stdin");
    }
}

fn bool_var(p: &str, args: &Args<'_>) -> bool {
    args.get_bool(p)
}

fn get_prelude(args: &mut Args<'_>) -> String {
    let env_file = Path::new("env.rs");
    // eprintln!("env path={env:?}, env exists={}", env.exists());
    let env_prelude = if env_file.exists() {
        let contents = read_rs_file_contents(env_file);
        eprintln!("contents={contents}");
        update_args_from_src("env.rs", &contents, args);
        Some(contents)
    } else {
        None
    };

    let mut prelude = cache::get_prelude();
    if let Some(env_prelude) = env_prelude {
        prelude.insert_str(0, &env_prelude);
    }
    prelude
}

fn get_rs_file_contents(args: &mut Args<'_>) -> Option<String> {
    let file_contents = if let Ok(program) = args.get_string_result("program") {
        let rs_file = Path::new(&program);
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        if program.ends_with(".rs") {
            if args.get_bool("compile-only") && args.get_bool("stdin") {
                None
            } else if rs_file.is_file() {
                args.clear_used();
                let contents = read_rs_file_contents(rs_file);
                let rs_file_name = rs_file.file_name()?.to_str()?;

                update_args_from_src(rs_file_name, &contents, args);
                Some(contents)
            } else {
                args.quit("file does not exist");
            }
        } else {
            None
        }
    } else {
        None
    };
    file_contents
}
